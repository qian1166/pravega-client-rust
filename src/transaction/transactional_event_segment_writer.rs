//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

use crate::client_factory::ClientFactoryInternal;
use crate::error::*;
use crate::event_stream_writer::{EventSegmentWriter, Incoming, PendingEvent};
use log::{debug, error, warn};
use pravega_rust_client_retry::retry_policy::RetryWithBackoff;
use pravega_rust_client_shared::ScopedSegment;
use pravega_wire_protocol::commands::DataAppendedCommand;
use pravega_wire_protocol::wire_commands::Replies;
use snafu::ResultExt;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{channel, Receiver};
use tokio::sync::oneshot;

/// TransactionalEventSegmentWriter contains a EventSegmentWriter that writes to a specific
/// transaction segment.
pub(super) struct TransactionalEventSegmentWriter {
    segment: ScopedSegment,
    event_segment_writer: EventSegmentWriter,
    recevier: Receiver<Incoming>,
    // Only need to hold onto the lastest event since if any previous events failed, the last one will also fail
    outstanding: Option<oneshot::Receiver<Result<(), EventStreamWriterError>>>,
}

impl TransactionalEventSegmentWriter {
    const CHANNEL_CAPACITY: usize = 100;

    pub(super) fn new(segment: ScopedSegment, retry_policy: RetryWithBackoff) -> Self {
        let (tx, rx) = channel(TransactionalEventSegmentWriter::CHANNEL_CAPACITY);
        let event_segment_writer = EventSegmentWriter::new(segment.clone(), tx, retry_policy);
        TransactionalEventSegmentWriter {
            segment,
            event_segment_writer,
            recevier: rx,
            outstanding: None,
        }
    }

    /// sets up connection for this transaction segment by sending wirecommand SetupAppend.
    /// If an error happened, try to reconnect to the server.
    pub(super) async fn initialize(&mut self, factory: &ClientFactoryInternal) {
        if let Err(_e) = self.event_segment_writer.setup_connection(factory).await {
            self.event_segment_writer.reconnect(factory).await;
        }
    }

    /// writes event to the sever by calling event_segment_writer.
    pub(super) async fn write_event(
        &mut self,
        event: Vec<u8>,
        factory: &ClientFactoryInternal,
    ) -> Result<(), TransactionalEventSegmentWriterError> {
        self.try_process_unacked_events(factory).await?;
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        if let Some(pending_event) = PendingEvent::with_header(None, event, oneshot_tx) {
            self.event_segment_writer
                .write(pending_event)
                .await
                .context(WriterError {})?;
        }
        // set the latest outstanding event.
        self.outstanding = Some(oneshot_rx);
        Ok(())
    }

    /// wait until all the outstanding events has been acked.
    pub(super) async fn flush(
        &mut self,
        factory: &ClientFactoryInternal,
    ) -> Result<(), TransactionalEventSegmentWriterError> {
        if self.outstanding.is_some() {
            self.process_unacked_events(factory).await?;
            self.remove_completed()?;
        }
        Ok(())
    }

    /// process any events that are waiting for server acks. It will wait until responses arrive.
    async fn process_unacked_events(
        &mut self,
        factory: &ClientFactoryInternal,
    ) -> Result<(), TransactionalEventSegmentWriterError> {
        if let Some(event) = self.recevier.recv().await {
            self.process_server_reply(event, factory).await?;
            Ok(())
        } else {
            Err(TransactionalEventSegmentWriterError::MpscSenderDropped {})
        }
    }

    /// try to process events that are waiting for server acks. If there is no response from server
    /// then return immediately.
    async fn try_process_unacked_events(
        &mut self,
        factory: &ClientFactoryInternal,
    ) -> Result<(), TransactionalEventSegmentWriterError> {
        loop {
            match self.recevier.try_recv() {
                Ok(event) => self.process_server_reply(event, factory).await?,
                // No reply from the server yet, just return ok.
                Err(TryRecvError::Empty) => return Ok(()),
                Err(e) => return Err(TransactionalEventSegmentWriterError::MpscError { source: e }),
            }
        }
    }

    /// processes the sever reply.
    async fn process_server_reply(
        &mut self,
        income: Incoming,
        factory: &ClientFactoryInternal,
    ) -> Result<(), TransactionalEventSegmentWriterError> {
        if let Incoming::ServerReply(reply) = income {
            if let Replies::DataAppended(cmd) = reply.reply {
                self.process_data_appended(factory, cmd).await;
                Ok(())
            } else {
                error!(
                    "unexpected reply from server, transaction failed due to {:?}",
                    reply
                );
                Err(TransactionalEventSegmentWriterError::UnexpectedReply { error: reply.reply })
            }
        } else {
            panic!("should always receive ServerReply type");
        }
    }

    /// processes the data appended wirecommand.
    async fn process_data_appended(&mut self, factory: &ClientFactoryInternal, cmd: DataAppendedCommand) {
        debug!(
            "data appended for writer {:?}, latest event id is: {:?}",
            self.event_segment_writer.get_uuid(),
            cmd.event_number
        );

        self.event_segment_writer.ack(cmd.event_number);
        if let Err(e) = self.event_segment_writer.write_pending_events().await {
            warn!(
                "writer {:?} failed to flush data to segment {:?} due to {:?}, reconnecting",
                self.event_segment_writer.get_uuid(),
                self.event_segment_writer.get_segment_name(),
                e
            );
            self.event_segment_writer.reconnect(factory).await;
        }
    }

    /// check if the latest inflight event has completed. If it has completed then there are no
    /// more inflight events.
    fn remove_completed(&mut self) -> Result<(), TransactionalEventSegmentWriterError> {
        assert!(self.outstanding.is_some());

        let mut rx = self.outstanding.take().expect("outstanding event is not empty");
        match rx.try_recv() {
            Ok(reply) => reply.context(WriterError {}),
            Err(oneshot::error::TryRecvError::Empty) => {
                // the outstanding event hasn't been acked, keep the Receiver and return ok
                self.outstanding = Some(rx);
                Ok(())
            }
            Err(e) => Err(TransactionalEventSegmentWriterError::OneshotError { source: e }),
        }
    }
}