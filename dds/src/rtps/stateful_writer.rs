use super::{
    behavior_types::Duration,
    error::RtpsResult,
    message_receiver::MessageReceiver,
    message_sender::{Clock, WriteMessage},
    reader_proxy::RtpsReaderProxy,
};
use crate::{
    rtps_messages::{
        overall_structure::{RtpsMessageRead, RtpsMessageWrite, RtpsSubmessageReadKind},
        submessage_elements::{ParameterList, SequenceNumberSet, SerializedDataFragment},
        submessages::{
            ack_nack::AckNackSubmessage, data_frag::DataFragSubmessage, gap::GapSubmessage,
            info_destination::InfoDestinationSubmessage, info_timestamp::InfoTimestampSubmessage,
            nack_frag::NackFragSubmessage,
        },
        types::TIME_INVALID,
    },
    transport::{
        history_cache::CacheChange,
        types::{
            ChangeKind, DurabilityKind, EntityId, Guid, GuidPrefix, ReliabilityKind,
            SequenceNumber, ENTITYID_UNKNOWN,
        },
        writer::ReaderProxy,
    },
};
use alloc::vec::Vec;

pub struct RtpsStatefulWriter {
    guid: Guid,
    changes: Vec<CacheChange>,
    matched_readers: Vec<RtpsReaderProxy>,
    heartbeat_period: Duration,
    data_max_size_serialized: usize,
}

impl RtpsStatefulWriter {
    pub fn new(guid: Guid, data_max_size_serialized: usize) -> Self {
        Self {
            guid,
            changes: Vec::new(),
            matched_readers: Vec::new(),
            heartbeat_period: Duration::from_millis(200),
            data_max_size_serialized,
        }
    }

    pub fn guid(&self) -> Guid {
        self.guid
    }

    pub fn data_max_size_serialized(&self) -> usize {
        self.data_max_size_serialized
    }

    pub fn add_change(&mut self, cache_change: CacheChange) {
        self.changes.push(cache_change);
    }

    pub fn remove_change(&mut self, sequence_number: SequenceNumber) {
        self.changes
            .retain(|cc| cc.sequence_number() != sequence_number);
    }

    pub fn is_change_acknowledged(&self, sequence_number: SequenceNumber) -> bool {
        !self
            .matched_readers
            .iter()
            .filter(|rp| rp.reliability() == ReliabilityKind::Reliable)
            .any(|rp| rp.unacked_changes(Some(sequence_number)))
    }

    pub fn add_matched_reader(&mut self, reader_proxy: &ReaderProxy) {
        let first_relevant_sample_seq_num = match reader_proxy.durability_kind {
            DurabilityKind::Volatile => self
                .changes
                .iter()
                .map(|cc| cc.sequence_number)
                .max()
                .unwrap_or(0),
            DurabilityKind::TransientLocal
            | DurabilityKind::Transient
            | DurabilityKind::Persistent => 0,
        };
        let rtps_reader_proxy = RtpsReaderProxy::new(
            reader_proxy.remote_reader_guid,
            reader_proxy.remote_group_entity_id,
            &reader_proxy.unicast_locator_list,
            &reader_proxy.multicast_locator_list,
            reader_proxy.expects_inline_qos,
            true,
            reader_proxy.reliability_kind,
            first_relevant_sample_seq_num,
            reader_proxy.durability_kind,
        );
        if let Some(rp) = self
            .matched_readers
            .iter_mut()
            .find(|rp| rp.remote_reader_guid() == reader_proxy.remote_reader_guid)
        {
            *rp = rtps_reader_proxy;
        } else {
            self.matched_readers.push(rtps_reader_proxy);
        }
    }

    pub fn delete_matched_reader(&mut self, reader_guid: Guid) {
        self.matched_readers
            .retain(|rp| rp.remote_reader_guid() != reader_guid);
    }

    pub async fn write_message(&mut self, message_writer: &impl WriteMessage, clock: &impl Clock) {
        for reader_proxy in &mut self.matched_readers {
            match reader_proxy.reliability() {
                ReliabilityKind::BestEffort => {
                    write_message_to_reader_proxy_best_effort(
                        reader_proxy,
                        self.guid.entity_id(),
                        &self.changes,
                        self.data_max_size_serialized,
                        message_writer,
                    )
                    .await
                }
                ReliabilityKind::Reliable => {
                    write_message_to_reader_proxy_reliable(
                        reader_proxy,
                        self.guid.entity_id(),
                        &self.changes,
                        self.changes.iter().map(|cc| cc.sequence_number()).min(),
                        self.changes.iter().map(|cc| cc.sequence_number()).max(),
                        self.data_max_size_serialized,
                        self.heartbeat_period,
                        message_writer,
                        clock,
                    )
                    .await
                }
            }
        }
    }

    pub async fn on_acknack_submessage_received(
        &mut self,
        acknack_submessage: &AckNackSubmessage,
        source_guid_prefix: GuidPrefix,
        message_writer: &impl WriteMessage,
        clock: &impl Clock,
    ) {
        if &self.guid.entity_id() == acknack_submessage.writer_id() {
            let reader_guid = Guid::new(source_guid_prefix, *acknack_submessage.reader_id());

            if let Some(reader_proxy) = self
                .matched_readers
                .iter_mut()
                .find(|x| x.remote_reader_guid() == reader_guid)
            {
                if reader_proxy.reliability() == ReliabilityKind::Reliable
                    && acknack_submessage.count() > reader_proxy.last_received_acknack_count()
                {
                    reader_proxy.acked_changes_set(acknack_submessage.reader_sn_state().base() - 1);
                    reader_proxy.requested_changes_set(acknack_submessage.reader_sn_state().set());

                    reader_proxy.set_last_received_acknack_count(acknack_submessage.count());

                    write_message_to_reader_proxy_reliable(
                        reader_proxy,
                        self.guid.entity_id(),
                        &self.changes,
                        self.changes.iter().map(|cc| cc.sequence_number()).min(),
                        self.changes.iter().map(|cc| cc.sequence_number()).max(),
                        self.data_max_size_serialized,
                        self.heartbeat_period,
                        message_writer,
                        clock,
                    )
                    .await;
                }
            }
        }
    }

    pub async fn on_nack_frag_submessage_received(
        &mut self,
        nackfrag_submessage: &NackFragSubmessage,
        source_guid_prefix: GuidPrefix,
        message_writer: &impl WriteMessage,
        clock: &impl Clock,
    ) {
        let reader_guid = Guid::new(source_guid_prefix, nackfrag_submessage.reader_id());

        if let Some(reader_proxy) = self
            .matched_readers
            .iter_mut()
            .find(|x| x.remote_reader_guid() == reader_guid)
        {
            if reader_proxy.reliability() == ReliabilityKind::Reliable
                && nackfrag_submessage.count() > reader_proxy.last_received_nack_frag_count()
            {
                reader_proxy
                    .requested_changes_set(core::iter::once(nackfrag_submessage.writer_sn()));
                reader_proxy.set_last_received_nack_frag_count(nackfrag_submessage.count());

                write_message_to_reader_proxy_reliable(
                    reader_proxy,
                    self.guid.entity_id(),
                    &self.changes,
                    self.changes.iter().map(|cc| cc.sequence_number()).min(),
                    self.changes.iter().map(|cc| cc.sequence_number()).max(),
                    self.data_max_size_serialized,
                    self.heartbeat_period,
                    message_writer,
                    clock,
                )
                .await;
            }
        }
    }

    pub async fn process_message(
        &mut self,
        datagram: &[u8],
        message_writer: &impl WriteMessage,
        clock: &impl Clock,
    ) -> RtpsResult<()> {
        let rtps_message = RtpsMessageRead::try_from(datagram)?;
        let mut message_receiver = MessageReceiver::new(&rtps_message);

        while let Some(submessage) = message_receiver.next() {
            match &submessage {
                RtpsSubmessageReadKind::AckNack(acknack_submessage) => {
                    self.on_acknack_submessage_received(
                        acknack_submessage,
                        message_receiver.source_guid_prefix(),
                        message_writer,
                        clock,
                    )
                    .await;
                }
                RtpsSubmessageReadKind::NackFrag(nackfrag_submessage) => {
                    self.on_nack_frag_submessage_received(
                        nackfrag_submessage,
                        message_receiver.source_guid_prefix(),
                        message_writer,
                        clock,
                    )
                    .await;
                }
                _ => (),
            }
        }
        Ok(())
    }
}

async fn write_message_to_reader_proxy_best_effort(
    reader_proxy: &mut RtpsReaderProxy,
    writer_id: EntityId,
    changes: &[CacheChange],
    data_max_size_serialized: usize,
    message_writer: &impl WriteMessage,
) {
    // a_change_seq_num := the_reader_proxy.next_unsent_change();
    // if ( a_change_seq_num > the_reader_proxy.higuest_sent_seq_num +1 ) {
    //      GAP = new GAP(the_reader_locator.higuest_sent_seq_num + 1, a_change_seq_num -1);
    //      GAP.readerId := ENTITYID_UNKNOWN;
    //      GAP.filteredCount := 0;
    //      send GAP;
    // }
    // a_change := the_writer.writer_cache.get_change(a_change_seq_num );
    // if ( DDS_FILTER(the_reader_proxy, a_change) ) {
    //      DATA = new DATA(a_change);
    //      IF (the_reader_proxy.expectsInlineQos) {
    //          DATA.inlineQos := the_rtps_writer.related_dds_writer.qos;
    //          DATA.inlineQos += a_change.inlineQos;
    //      }
    //      DATA.readerId := ENTITYID_UNKNOWN;
    //      send DATA;
    // }
    // else {
    //      GAP = new GAP(a_change.sequenceNumber);
    //      GAP.readerId := ENTITYID_UNKNOWN;
    //      GAP.filteredCount := 1;
    //      send GAP;
    // }
    // the_reader_proxy.higuest_sent_seq_num := a_change_seq_num;
    while let Some(next_unsent_change_seq_num) = reader_proxy.next_unsent_change(changes.iter()) {
        if next_unsent_change_seq_num > reader_proxy.highest_sent_seq_num() + 1 {
            let gap_start_sequence_number = reader_proxy.highest_sent_seq_num() + 1;
            let gap_end_sequence_number = next_unsent_change_seq_num - 1;
            let gap_submessage = GapSubmessage::new(
                reader_proxy.remote_reader_guid().entity_id(),
                writer_id,
                gap_start_sequence_number,
                SequenceNumberSet::new(gap_end_sequence_number + 1, []),
            );
            let rtps_message = RtpsMessageWrite::from_submessages(
                &[&gap_submessage],
                message_writer.guid_prefix(),
            );
            message_writer
                .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                .await;

            reader_proxy.set_highest_sent_seq_num(next_unsent_change_seq_num);
        } else if let Some(cache_change) = changes
            .iter()
            .find(|cc| cc.sequence_number() == next_unsent_change_seq_num)
        {
            let number_of_fragments = cache_change
                .data_value()
                .len()
                .div_ceil(data_max_size_serialized);

            // Either send a DATAFRAG submessages or send a single DATA submessage
            if number_of_fragments > 1 {
                for frag_index in 0..number_of_fragments {
                    let info_dst =
                        InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

                    let info_timestamp = if let Some(timestamp) = cache_change.source_timestamp() {
                        InfoTimestampSubmessage::new(false, timestamp.into())
                    } else {
                        InfoTimestampSubmessage::new(true, TIME_INVALID)
                    };

                    let inline_qos_flag = true;
                    let key_flag = match cache_change.kind() {
                        ChangeKind::Alive => false,
                        ChangeKind::NotAliveDisposed | ChangeKind::NotAliveUnregistered => true,
                        _ => todo!(),
                    };
                    let non_standard_payload_flag = false;
                    let reader_id = reader_proxy.remote_reader_guid().entity_id();
                    let writer_sn = cache_change.sequence_number();
                    let fragment_starting_num = (frag_index + 1) as u32;
                    let fragments_in_submessage = 1;
                    let fragment_size = data_max_size_serialized as u16;
                    let data_size = cache_change.data_value().len() as u32;

                    let start = frag_index * data_max_size_serialized;
                    let end = core::cmp::min(
                        (frag_index + 1) * data_max_size_serialized,
                        cache_change.data_value().len(),
                    );

                    let serialized_payload = SerializedDataFragment::new(
                        cache_change.data_value().clone().into(),
                        start..end,
                    );

                    let data_frag = DataFragSubmessage::new(
                        inline_qos_flag,
                        non_standard_payload_flag,
                        key_flag,
                        reader_id,
                        writer_id,
                        writer_sn,
                        fragment_starting_num,
                        fragments_in_submessage,
                        fragment_size,
                        data_size,
                        ParameterList::new(Vec::new()),
                        serialized_payload,
                    );
                    let rtps_message = RtpsMessageWrite::from_submessages(
                        &[&info_dst, &info_timestamp, &data_frag],
                        message_writer.guid_prefix(),
                    );
                    message_writer
                        .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                        .await;
                }
            } else {
                let info_dst =
                    InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

                let info_timestamp = if let Some(timestamp) = cache_change.source_timestamp() {
                    InfoTimestampSubmessage::new(false, timestamp.into())
                } else {
                    InfoTimestampSubmessage::new(true, TIME_INVALID)
                };

                let data_submessage = cache_change
                    .as_data_submessage(reader_proxy.remote_reader_guid().entity_id(), writer_id);

                let rtps_message = RtpsMessageWrite::from_submessages(
                    &[&info_dst, &info_timestamp, &data_submessage],
                    message_writer.guid_prefix(),
                );
                message_writer
                    .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                    .await;
            }
        } else {
            let gap_submessage = GapSubmessage::new(
                ENTITYID_UNKNOWN,
                writer_id,
                next_unsent_change_seq_num,
                SequenceNumberSet::new(next_unsent_change_seq_num + 1, []),
            );
            let rtps_message = RtpsMessageWrite::from_submessages(
                &[&gap_submessage],
                message_writer.guid_prefix(),
            );
            message_writer
                .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                .await;
        }

        reader_proxy.set_highest_sent_seq_num(next_unsent_change_seq_num);
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_message_to_reader_proxy_reliable(
    reader_proxy: &mut RtpsReaderProxy,
    writer_id: EntityId,
    changes: &[CacheChange],
    seq_num_min: Option<SequenceNumber>,
    seq_num_max: Option<SequenceNumber>,
    data_max_size_serialized: usize,
    heartbeat_period: Duration,
    message_writer: &impl WriteMessage,
    clock: &impl Clock,
) {
    let now = clock.now();
    // Top part of the state machine - Figure 8.19 RTPS standard
    if reader_proxy.unsent_changes(changes.iter()) {
        while let Some(next_unsent_change_seq_num) = reader_proxy.next_unsent_change(changes.iter())
        {
            if next_unsent_change_seq_num > reader_proxy.highest_sent_seq_num() + 1 {
                let gap_start_sequence_number = reader_proxy.highest_sent_seq_num() + 1;
                let gap_end_sequence_number = next_unsent_change_seq_num - 1;
                let gap_submessage = GapSubmessage::new(
                    reader_proxy.remote_reader_guid().entity_id(),
                    writer_id,
                    gap_start_sequence_number,
                    SequenceNumberSet::new(gap_end_sequence_number + 1, []),
                );
                let first_sn = seq_num_min.unwrap_or(1);
                let last_sn = seq_num_max.unwrap_or(0);
                let heartbeat_submessage = reader_proxy
                    .heartbeat_machine()
                    .generate_new_heartbeat(writer_id, first_sn, last_sn, now, false);
                let info_dst =
                    InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());
                let rtps_message = RtpsMessageWrite::from_submessages(
                    &[&info_dst, &gap_submessage, &heartbeat_submessage],
                    message_writer.guid_prefix(),
                );
                message_writer
                    .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                    .await;
            } else {
                write_change_message_reader_proxy_reliable(
                    reader_proxy,
                    writer_id,
                    changes,
                    seq_num_min,
                    seq_num_max,
                    data_max_size_serialized,
                    next_unsent_change_seq_num,
                    message_writer,
                    clock,
                )
                .await;
            }
            reader_proxy.set_highest_sent_seq_num(next_unsent_change_seq_num);
        }
    } else if !reader_proxy.unacked_changes(seq_num_max) {
        // Idle
        if reader_proxy
            .heartbeat_machine()
            .is_time_for_heartbeat(now, heartbeat_period.into())
            && reader_proxy.durability() != DurabilityKind::Volatile
        {
            let first_sn = seq_num_min.unwrap_or(1);
            let last_sn = seq_num_max.unwrap_or(0);
            let heartbeat_submessage = reader_proxy
                .heartbeat_machine()
                .generate_new_heartbeat(writer_id, first_sn, last_sn, now, true);

            let info_dst =
                InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

            let rtps_message = RtpsMessageWrite::from_submessages(
                &[&info_dst, &heartbeat_submessage],
                message_writer.guid_prefix(),
            );
            message_writer
                .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                .await;
        }
    } else if reader_proxy
        .heartbeat_machine()
        .is_time_for_heartbeat(now, heartbeat_period.into())
    {
        let first_sn = seq_num_min.unwrap_or(1);
        let last_sn = seq_num_max.unwrap_or(0);
        let heartbeat_submessage = reader_proxy
            .heartbeat_machine()
            .generate_new_heartbeat(writer_id, first_sn, last_sn, now, false);

        let info_dst = InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

        let rtps_message = RtpsMessageWrite::from_submessages(
            &[&info_dst, &heartbeat_submessage],
            message_writer.guid_prefix(),
        );
        message_writer
            .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
            .await;
    }

    // Middle-part of the state-machine - Figure 8.19 RTPS standard
    if !reader_proxy.requested_changes().is_empty() {
        while let Some(next_requested_change_seq_num) = reader_proxy.next_requested_change() {
            // "a_change.status := UNDERWAY;" should be done by next_requested_change() as
            // it's not done here to avoid the change being a mutable reference
            // Also the post-condition:
            // a_change BELONGS-TO the_reader_proxy.requested_changes() ) == FALSE
            // should be full-filled by next_requested_change()
            write_change_message_reader_proxy_reliable(
                reader_proxy,
                writer_id,
                changes,
                seq_num_min,
                seq_num_max,
                data_max_size_serialized,
                next_requested_change_seq_num,
                message_writer,
                clock,
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_change_message_reader_proxy_reliable(
    reader_proxy: &mut RtpsReaderProxy,
    writer_id: EntityId,
    changes: &[CacheChange],
    seq_num_min: Option<SequenceNumber>,
    seq_num_max: Option<SequenceNumber>,
    data_max_size_serialized: usize,
    change_seq_num: SequenceNumber,
    message_writer: &impl WriteMessage,
    clock: &impl Clock,
) {
    let now = clock.now();
    match changes
        .iter()
        .find(|cc| cc.sequence_number() == change_seq_num)
    {
        Some(cache_change) if change_seq_num > reader_proxy.first_relevant_sample_seq_num() => {
            let number_of_fragments = cache_change
                .data_value()
                .len()
                .div_ceil(data_max_size_serialized);

            // Either send a DATAFRAG submessages or send a single DATA submessage
            if number_of_fragments > 1 {
                for frag_index in 0..number_of_fragments {
                    let info_dst =
                        InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

                    let info_timestamp = if let Some(timestamp) = cache_change.source_timestamp() {
                        InfoTimestampSubmessage::new(false, timestamp.into())
                    } else {
                        InfoTimestampSubmessage::new(true, TIME_INVALID)
                    };

                    let inline_qos_flag = true;
                    let key_flag = match cache_change.kind() {
                        ChangeKind::Alive => false,
                        ChangeKind::NotAliveDisposed | ChangeKind::NotAliveUnregistered => true,
                        _ => todo!(),
                    };
                    let non_standard_payload_flag = false;
                    let reader_id = reader_proxy.remote_reader_guid().entity_id();
                    let writer_sn = cache_change.sequence_number();
                    let fragment_starting_num = (frag_index + 1) as u32;
                    let fragments_in_submessage = 1;
                    let fragment_size = data_max_size_serialized as u16;
                    let data_size = cache_change.data_value().len() as u32;

                    let start = frag_index * data_max_size_serialized;
                    let end = core::cmp::min(
                        (frag_index + 1) * data_max_size_serialized,
                        cache_change.data_value().len(),
                    );

                    let serialized_payload = SerializedDataFragment::new(
                        cache_change.data_value().clone().into(),
                        start..end,
                    );

                    let data_frag = DataFragSubmessage::new(
                        inline_qos_flag,
                        non_standard_payload_flag,
                        key_flag,
                        reader_id,
                        writer_id,
                        writer_sn,
                        fragment_starting_num,
                        fragments_in_submessage,
                        fragment_size,
                        data_size,
                        ParameterList::new(Vec::new()),
                        serialized_payload,
                    );

                    let rtps_message = RtpsMessageWrite::from_submessages(
                        &[&info_dst, &info_timestamp, &data_frag],
                        message_writer.guid_prefix(),
                    );
                    message_writer
                        .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                        .await;
                }
            } else {
                let info_dst =
                    InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

                let info_timestamp = if let Some(timestamp) = cache_change.source_timestamp() {
                    InfoTimestampSubmessage::new(false, timestamp.into())
                } else {
                    InfoTimestampSubmessage::new(true, TIME_INVALID)
                };

                let data_submessage = cache_change
                    .as_data_submessage(reader_proxy.remote_reader_guid().entity_id(), writer_id);

                let first_sn = seq_num_min.unwrap_or(1);
                let last_sn = seq_num_max.unwrap_or(0);
                let heartbeat = reader_proxy
                    .heartbeat_machine()
                    .generate_new_heartbeat(writer_id, first_sn, last_sn, now, false);

                let rtps_message = RtpsMessageWrite::from_submessages(
                    &[&info_dst, &info_timestamp, &data_submessage, &heartbeat],
                    message_writer.guid_prefix(),
                );
                message_writer
                    .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                    .await;
            }
        }
        _ => {
            let info_dst =
                InfoDestinationSubmessage::new(reader_proxy.remote_reader_guid().prefix());

            let gap_submessage = GapSubmessage::new(
                ENTITYID_UNKNOWN,
                writer_id,
                change_seq_num,
                SequenceNumberSet::new(change_seq_num + 1, []),
            );

            let rtps_message = RtpsMessageWrite::from_submessages(
                &[&info_dst, &gap_submessage],
                message_writer.guid_prefix(),
            );
            message_writer
                .write_message(rtps_message.buffer(), reader_proxy.unicast_locator_list())
                .await;
        }
    }
}
