/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use jmap_proto::types::{collection::Collection, property::Property};
use store::{
    ahash::{AHashMap, AHashSet},
    fts::index::FtsDocument,
    write::{
        key::DeserializeBigEndian, now, BatchBuilder, Bincode, FtsQueueClass, MaybeDynamicId,
        ValueClass,
    },
    Deserialize, IterateParams, Serialize, ValueKey, U32_LEN, U64_LEN,
};

use utils::{BlobHash, BLOB_HASH_LEN};

use crate::{
    email::{index::IndexMessageText, metadata::MessageMetadata},
    JMAP,
};

use super::housekeeper::Event;

#[derive(Debug)]
struct IndexEmail {
    account_id: u32,
    document_id: u32,
    seq: u64,
    lock_expiry: u64,
    insert_hash: Option<BlobHash>,
}

const INDEX_LOCK_EXPIRY: u64 = 60 * 5;

impl JMAP {
    pub async fn fts_index_queued(&self) {
        let from_key = ValueKey::<ValueClass<u32>> {
            account_id: 0,
            collection: 0,
            document_id: 0,
            class: ValueClass::FtsQueue(FtsQueueClass::Delete { seq: 0 }),
        };
        let to_key = ValueKey::<ValueClass<u32>> {
            account_id: u32::MAX,
            collection: u8::MAX,
            document_id: u32::MAX,
            class: ValueClass::FtsQueue(FtsQueueClass::Delete { seq: u64::MAX }),
        };

        // Retrieve entries pending to be indexed
        let mut insert_entries = Vec::new();
        let mut delete_entries: AHashMap<u32, Vec<IndexEmail>> = AHashMap::new();
        let mut skipped_documents = AHashSet::new();
        let now = now();
        let _ = self
            .core
            .storage
            .data
            .iterate(
                IterateParams::new(from_key, to_key).ascending(),
                |key, value| {
                    let event = IndexEmail::deserialize(key, value)?;

                    if event.lock_expiry < now {
                        if !skipped_documents.contains(&(event.account_id, event.document_id)) {
                            if event.insert_hash.is_some() {
                                insert_entries.push(event);
                            } else {
                                delete_entries
                                    .entry(event.account_id)
                                    .or_default()
                                    .push(event);
                            }
                        } else {
                            tracing::trace!(
                                context = "queue",
                                event = "skipped",
                                account_id = event.account_id,
                                document_id = event.document_id,
                                "DocumentId already locked by another process."
                            );
                        }
                    } else {
                        skipped_documents.insert((event.account_id, event.document_id));
                        tracing::trace!(
                            context = "queue",
                            event = "locked",
                            account_id = event.account_id,
                            document_id = event.document_id,
                            expiry = event.lock_expiry - now,
                            "Index event locked by another process."
                        );
                    }

                    Ok(true)
                },
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    context = "fts_index_queued",
                    event = "error",
                    reason = ?err,
                    "Failed to iterate over index emails"
                );
            });

        // Add entries to the index
        for event in insert_entries {
            // Lock index
            if !self.try_lock_index(&event).await {
                continue;
            }

            match self
                .get_property::<Bincode<MessageMetadata>>(
                    event.account_id,
                    Collection::Email,
                    event.document_id,
                    Property::BodyStructure,
                )
                .await
            {
                Ok(Some(metadata))
                    if metadata.inner.blob_hash.as_slice()
                        == event.insert_hash.as_ref().unwrap().as_slice() =>
                {
                    // Obtain raw message
                    let raw_message = if let Ok(Some(raw_message)) = self
                        .get_blob(&metadata.inner.blob_hash, 0..usize::MAX)
                        .await
                    {
                        raw_message
                    } else {
                        tracing::warn!(
                            context = "fts_index_queued",
                            event = "error",
                            account_id = event.account_id,
                            document_id = event.document_id,
                            blob_hash = ?metadata.inner.blob_hash,
                            "Message blob not found"
                        );
                        continue;
                    };
                    let message = metadata.inner.contents.into_message(&raw_message);

                    // Index message
                    let document =
                        FtsDocument::with_default_language(self.core.jmap.default_language)
                            .with_account_id(event.account_id)
                            .with_collection(Collection::Email)
                            .with_document_id(event.document_id)
                            .index_message(&message);
                    if let Err(err) = self.core.storage.fts.index(document).await {
                        tracing::error!(
                            context = "fts_index_queued",
                            event = "error",
                            account_id = event.account_id,
                            document_id = event.document_id,
                            reason = ?err,
                            "Failed to index email in FTS index"
                        );
                        continue;
                    }

                    tracing::debug!(
                        context = "fts_index_queued",
                        event = "index",
                        account_id = event.account_id,
                        document_id = event.document_id,
                        "Indexed document in FTS index"
                    );
                }

                Err(err) => {
                    tracing::error!(
                        context = "fts_index_queued",
                        event = "error",
                        account_id = event.account_id,
                        document_id = event.document_id,
                        reason = ?err,
                        "Failed to retrieve email metadata"
                    );
                    break;
                }
                _ => {
                    // The message was probably deleted or overwritten
                    tracing::debug!(
                        context = "fts_index_queued",
                        event = "error",
                        account_id = event.account_id,
                        document_id = event.document_id,
                        "Email metadata not found"
                    );
                }
            }

            // Remove entry from queue
            if let Err(err) = self
                .core
                .storage
                .data
                .write(
                    BatchBuilder::new()
                        .with_account_id(event.account_id)
                        .with_collection(Collection::Email)
                        .update_document(event.document_id)
                        .clear(event.value_class())
                        .build_batch(),
                )
                .await
            {
                tracing::error!(
                    context = "fts_index_queued",
                    event = "error",
                    reason = ?err,
                    "Failed to remove index email from queue"
                );
                break;
            }
        }

        // Remove entries from the index
        for (account_id, events) in delete_entries {
            let mut document_ids = Vec::with_capacity(events.len());
            let mut locked_events = Vec::with_capacity(events.len());

            for event in events {
                if self.try_lock_index(&event).await {
                    document_ids.push(event.document_id);
                    locked_events.push(event);
                }
            }

            if document_ids.is_empty() {
                continue;
            }

            if let Err(err) = self
                .core
                .storage
                .fts
                .remove(account_id, Collection::Email.into(), &document_ids)
                .await
            {
                tracing::error!(
                    context = "fts_index_queued",
                    event = "error",
                    account_id = account_id,
                    document_ids = ?document_ids,
                    reason = ?err,
                    "Failed to remove documents from FTS index"
                );
                continue;
            }

            // Remove entries from the queue
            let mut batch = BatchBuilder::new();
            batch
                .with_account_id(account_id)
                .with_collection(Collection::Email);
            for event in locked_events {
                batch
                    .update_document(event.document_id)
                    .clear(event.value_class());
            }

            // Remove entry from queue
            if let Err(err) = self.core.storage.data.write(batch.build()).await {
                tracing::error!(
                    context = "fts_index_queued",
                    event = "error",
                    reason = ?err,
                    "Failed to remove index email from queue"
                );
                continue;
            }

            tracing::debug!(
                context = "fts_index_queued",
                event = "delete",
                account_id = account_id,
                document_ids = ?document_ids,
                "Deleted document from FTS index"
            );
        }

        if let Err(err) = self.inner.housekeeper_tx.send(Event::IndexDone).await {
            tracing::warn!("Failed to send index done event to housekeeper: {}", err);
        }
    }

    async fn try_lock_index(&self, event: &IndexEmail) -> bool {
        let mut batch = BatchBuilder::new();
        batch
            .with_account_id(event.account_id)
            .with_collection(Collection::Email)
            .update_document(event.document_id)
            .assert_value(event.value_class(), event.lock_expiry)
            .set(event.value_class(), (now() + INDEX_LOCK_EXPIRY).serialize());
        match self.core.storage.data.write(batch.build()).await {
            Ok(_) => true,
            Err(store::Error::AssertValueFailed) => {
                tracing::trace!(
                    context = "queue",
                    event = "locked",
                    account_id = event.account_id,
                    document_id = event.document_id,
                    "Lock busy: Index already locked."
                );
                false
            }
            Err(err) => {
                tracing::error!(
                    context = "queue",
                    event = "error",
                    "Failed to lock index: {}",
                    err
                );
                false
            }
        }
    }
}

impl IndexEmail {
    fn value_class(&self) -> ValueClass<MaybeDynamicId> {
        match &self.insert_hash {
            Some(hash) => ValueClass::FtsQueue(FtsQueueClass::Insert {
                hash: hash.clone(),
                seq: self.seq,
            }),
            None => ValueClass::FtsQueue(FtsQueueClass::Delete { seq: self.seq }),
        }
    }

    fn deserialize(key: &[u8], value: &[u8]) -> store::Result<Self> {
        Ok(IndexEmail {
            seq: key.deserialize_be_u64(0)?,
            account_id: key.deserialize_be_u32(U64_LEN)?,
            document_id: key.deserialize_be_u32(U64_LEN + U32_LEN + 1)?,
            lock_expiry: u64::deserialize(value)?,
            insert_hash: key
                .get(
                    U64_LEN + U32_LEN + U32_LEN + 1
                        ..U64_LEN + U32_LEN + U32_LEN + BLOB_HASH_LEN + 1,
                )
                .and_then(|bytes| BlobHash::try_from_hash_slice(bytes).ok()),
        })
    }
}
