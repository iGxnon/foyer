//  Copyright 2024 Foyer Project Authors
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

use std::{fmt::Debug, sync::Arc};

use foyer_common::code::{StorageKey, StorageValue};

use crate::{catalog::Catalog, metrics::Metrics};

#[derive(Debug)]
pub struct AdmissionContext<K, V>
where
    K: StorageKey,
    V: StorageValue,
{
    pub catalog: Arc<Catalog<K, V>>,
    pub metrics: Arc<Metrics>,
}

impl<K, V> Clone for AdmissionContext<K, V>
where
    K: StorageKey,
    V: StorageValue,
{
    fn clone(&self) -> Self {
        Self {
            catalog: self.catalog.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

pub trait AdmissionPolicy: Send + Sync + 'static + Debug {
    type Key: StorageKey;
    type Value: StorageValue;

    fn init(&self, context: AdmissionContext<Self::Key, Self::Value>);

    fn judge(&self, key: &Self::Key) -> bool;

    fn on_insert(&self, key: &Self::Key, judge: bool);

    fn on_drop(&self, key: &Self::Key, judge: bool);
}

pub mod rated_ticket;
