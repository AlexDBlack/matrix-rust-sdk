// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use dashmap::DashMap;

use matrix_sdk_common::locks::Mutex;
use tracing::{trace, warn};

use matrix_sdk_common::{
    events::{AnyToDeviceEvent, AnyToDeviceEventContent},
    identifiers::{DeviceId, UserId},
    uuid::Uuid,
};

use super::sas::{content_to_request, Sas, VerificationResult};
use crate::{
    olm::PrivateCrossSigningIdentity,
    requests::{OutgoingRequest, ToDeviceRequest},
    store::{CryptoStore, CryptoStoreError},
    ReadOnlyAccount, ReadOnlyDevice,
};

#[derive(Clone, Debug)]
pub struct VerificationMachine {
    account: ReadOnlyAccount,
    private_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    pub(crate) store: Arc<Box<dyn CryptoStore>>,
    verifications: Arc<DashMap<String, Sas>>,
    outgoing_to_device_messages: Arc<DashMap<Uuid, OutgoingRequest>>,
}

impl VerificationMachine {
    pub(crate) fn new(
        account: ReadOnlyAccount,
        identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        store: Arc<Box<dyn CryptoStore>>,
    ) -> Self {
        Self {
            account,
            private_identity: identity,
            store,
            verifications: Arc::new(DashMap::new()),
            outgoing_to_device_messages: Arc::new(DashMap::new()),
        }
    }

    pub async fn start_sas(
        &self,
        device: ReadOnlyDevice,
    ) -> Result<(Sas, ToDeviceRequest), CryptoStoreError> {
        let identity = self.store.get_user_identity(device.user_id()).await?;
        let private_identity = self.private_identity.lock().await.clone();

        let (sas, content) = Sas::start(
            self.account.clone(),
            private_identity,
            device.clone(),
            self.store.clone(),
            identity,
        );

        let request = content_to_request(
            device.user_id(),
            device.device_id(),
            AnyToDeviceEventContent::KeyVerificationStart(content),
        );

        self.verifications
            .insert(sas.flow_id().to_owned(), sas.clone());

        Ok((sas, request))
    }

    pub fn get_sas(&self, transaction_id: &str) -> Option<Sas> {
        #[allow(clippy::map_clone)]
        self.verifications.get(transaction_id).map(|s| s.clone())
    }

    fn queue_up_content(
        &self,
        recipient: &UserId,
        recipient_device: &DeviceId,
        content: AnyToDeviceEventContent,
    ) {
        let request = content_to_request(recipient, recipient_device, content);
        let request_id = request.txn_id;

        let request = OutgoingRequest {
            request_id,
            request: Arc::new(request.into()),
        };

        self.outgoing_to_device_messages.insert(request_id, request);
    }

    fn receive_event_helper(&self, sas: &Sas, event: &mut AnyToDeviceEvent) {
        if let Some(c) = sas.receive_event(event) {
            self.queue_up_content(sas.other_user_id(), sas.other_device_id(), c);
        }
    }

    pub fn mark_request_as_sent(&self, uuid: &Uuid) {
        self.outgoing_to_device_messages.remove(uuid);
    }

    pub fn outgoing_to_device_requests(&self) -> Vec<OutgoingRequest> {
        #[allow(clippy::map_clone)]
        self.outgoing_to_device_messages
            .iter()
            .map(|r| (*r).clone())
            .collect()
    }

    pub fn garbage_collect(&self) {
        self.verifications
            .retain(|_, s| !(s.is_done() || s.is_canceled()));

        for sas in self.verifications.iter() {
            if let Some(r) = sas.cancel_if_timed_out() {
                self.outgoing_to_device_messages.insert(
                    r.txn_id,
                    OutgoingRequest {
                        request_id: r.txn_id,
                        request: Arc::new(r.into()),
                    },
                );
            }
        }
    }

    pub async fn receive_event(
        &self,
        event: &mut AnyToDeviceEvent,
    ) -> Result<(), CryptoStoreError> {
        trace!("Received a key verification event {:?}", event);

        match event {
            AnyToDeviceEvent::KeyVerificationStart(e) => {
                trace!(
                    "Received a m.key.verification start event from {} {}",
                    e.sender,
                    e.content.from_device
                );

                if let Some(d) = self
                    .store
                    .get_device(&e.sender, &e.content.from_device)
                    .await?
                {
                    let private_identity = self.private_identity.lock().await.clone();
                    match Sas::from_start_event(
                        self.account.clone(),
                        private_identity,
                        d,
                        self.store.clone(),
                        e,
                        self.store.get_user_identity(&e.sender).await?,
                    ) {
                        Ok(s) => {
                            self.verifications
                                .insert(e.content.transaction_id.clone(), s);
                        }
                        Err(c) => {
                            warn!(
                                "Can't start key verification with {} {}, canceling: {:?}",
                                e.sender, e.content.from_device, c
                            );
                            self.queue_up_content(&e.sender, &e.content.from_device, c)
                        }
                    }
                } else {
                    warn!(
                        "Received a key verification start event from an unknown device {} {}",
                        e.sender, e.content.from_device
                    );
                }
            }
            AnyToDeviceEvent::KeyVerificationCancel(e) => {
                self.verifications.remove(&e.content.transaction_id);
            }
            AnyToDeviceEvent::KeyVerificationAccept(e) => {
                if let Some(s) = self.get_sas(&e.content.transaction_id) {
                    self.receive_event_helper(&s, event)
                };
            }
            AnyToDeviceEvent::KeyVerificationKey(e) => {
                if let Some(s) = self.get_sas(&e.content.transaction_id) {
                    self.receive_event_helper(&s, event)
                };
            }
            AnyToDeviceEvent::KeyVerificationMac(e) => {
                if let Some(s) = self.get_sas(&e.content.transaction_id) {
                    self.receive_event_helper(&s, event);

                    if s.is_done() {
                        match s.mark_as_done().await? {
                            VerificationResult::Ok => (),
                            VerificationResult::Cancel(r) => {
                                self.outgoing_to_device_messages.insert(
                                    r.txn_id,
                                    OutgoingRequest {
                                        request_id: r.txn_id,
                                        request: Arc::new(r.into()),
                                    },
                                );
                            }
                            VerificationResult::SignatureUpload(r) => {
                                let request_id = Uuid::new_v4();

                                self.outgoing_to_device_messages.insert(
                                    request_id,
                                    OutgoingRequest {
                                        request_id,
                                        request: Arc::new(r.into()),
                                    },
                                );
                            }
                        }
                    }
                };
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {

    use std::{
        convert::TryFrom,
        sync::Arc,
        time::{Duration, Instant},
    };

    use matrix_sdk_common::{
        events::AnyToDeviceEventContent,
        identifiers::{DeviceId, UserId},
        locks::Mutex,
    };

    use super::{Sas, VerificationMachine};
    use crate::{
        olm::PrivateCrossSigningIdentity,
        requests::OutgoingRequests,
        store::{CryptoStore, MemoryStore},
        verification::test::{get_content_from_request, wrap_any_to_device_content},
        ReadOnlyAccount, ReadOnlyDevice,
    };

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> Box<DeviceId> {
        "JLAFKJWSCS".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVCIE".into()
    }

    async fn setup_verification_machine() -> (VerificationMachine, Sas) {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let store = MemoryStore::new();
        let bob_store = MemoryStore::new();

        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        store.save_devices(vec![bob_device]).await;
        bob_store.save_devices(vec![alice_device.clone()]).await;

        let bob_store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(bob_store));
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let machine = VerificationMachine::new(alice, identity, Arc::new(Box::new(store)));
        let (bob_sas, start_content) = Sas::start(
            bob,
            PrivateCrossSigningIdentity::empty(bob_id()),
            alice_device,
            bob_store,
            None,
        );
        machine
            .receive_event(&mut wrap_any_to_device_content(
                bob_sas.user_id(),
                AnyToDeviceEventContent::KeyVerificationStart(start_content),
            ))
            .await
            .unwrap();

        (machine, bob_sas)
    }

    #[test]
    fn create() {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let store = MemoryStore::new();
        let _ = VerificationMachine::new(alice, identity, Arc::new(Box::new(store)));
    }

    #[tokio::test]
    async fn full_flow() {
        let (alice_machine, bob) = setup_verification_machine().await;

        let alice = alice_machine.get_sas(bob.flow_id()).unwrap();

        let mut event = alice
            .accept()
            .map(|c| wrap_any_to_device_content(alice.user_id(), get_content_from_request(&c)))
            .unwrap();

        let mut event = bob
            .receive_event(&mut event)
            .map(|c| wrap_any_to_device_content(bob.user_id(), c))
            .unwrap();

        assert!(alice_machine.outgoing_to_device_messages.is_empty());
        alice_machine.receive_event(&mut event).await.unwrap();
        assert!(!alice_machine.outgoing_to_device_messages.is_empty());

        let request = alice_machine
            .outgoing_to_device_messages
            .iter()
            .next()
            .unwrap();

        let txn_id = *request.request_id();

        let r = if let OutgoingRequests::ToDeviceRequest(r) = request.request() {
            r
        } else {
            panic!("Invalid request type");
        };

        let mut event = wrap_any_to_device_content(alice.user_id(), get_content_from_request(r));
        drop(request);
        alice_machine.mark_request_as_sent(&txn_id);

        assert!(bob.receive_event(&mut event).is_none());

        assert!(alice.emoji().is_some());
        assert!(bob.emoji().is_some());

        assert_eq!(alice.emoji(), bob.emoji());

        let mut event = wrap_any_to_device_content(
            alice.user_id(),
            get_content_from_request(&alice.confirm().await.unwrap().0.unwrap()),
        );
        bob.receive_event(&mut event);

        let mut event = wrap_any_to_device_content(
            bob.user_id(),
            get_content_from_request(&bob.confirm().await.unwrap().0.unwrap()),
        );
        alice.receive_event(&mut event);

        assert!(alice.is_done());
        assert!(bob.is_done());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timing_out() {
        let (alice_machine, bob) = setup_verification_machine().await;
        let alice = alice_machine.get_sas(bob.flow_id()).unwrap();

        assert!(!alice.timed_out());
        assert!(alice_machine.outgoing_to_device_messages.is_empty());

        // This line panics on macOS, so we're disabled for now.
        alice.set_creation_time(Instant::now() - Duration::from_secs(60 * 15));
        assert!(alice.timed_out());
        assert!(alice_machine.outgoing_to_device_messages.is_empty());
        alice_machine.garbage_collect();
        assert!(!alice_machine.outgoing_to_device_messages.is_empty());
        alice_machine.garbage_collect();
        assert!(alice_machine.verifications.is_empty());
    }
}
