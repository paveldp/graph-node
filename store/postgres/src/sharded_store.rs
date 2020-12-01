use std::{collections::BTreeMap, collections::HashMap, sync::Arc};
use std::{str::FromStr, sync::Mutex};

use diesel::{Connection, PgConnection};

use graph::{
    components::store,
    data::subgraph::schema::{MetadataType, SUBGRAPHS_ID},
    data::subgraph::status,
    prelude::{
        lazy_static, serde_json,
        web3::types::{Address, H256},
        ApiSchema, BlockNumber, DeploymentState, DynTryFuture, Entity, EntityKey,
        EntityModification, EntityQuery, Error, EthereumBlockPointer, EthereumCallCache, Logger,
        MetadataOperation, NodeId, QueryExecutionError, QueryStore, Schema, StopwatchMetrics,
        Store as StoreTrait, StoreError, StoreEvent, StoreEventStreamBox, SubgraphDeploymentEntity,
        SubgraphDeploymentId, SubgraphDeploymentStore, SubgraphEntityPair, SubgraphName,
        SubgraphVersionSwitchingMode, PRIMARY_SHARD,
    },
};
use store::StoredDynamicDataSource;

use crate::store::{ReplicaId, Store};
use crate::{deployment, notification_listener::JsonNotification, primary};

#[cfg(debug_assertions)]
lazy_static! {
    /// Tests set this to true so that `send_store_event` will store a copy
    /// of each event sent in `EVENT_TAP`
    pub static ref EVENT_TAP_ENABLED: Mutex<bool> = Mutex::new(false);
    pub static ref EVENT_TAP: Mutex<Vec<StoreEvent>> = Mutex::new(Vec::new());
}

/// Multiplex store operations on subgraphs and deployments between a primary
/// and any number of additional storage shards. See [this document](../../docs/sharded.md)
/// for details on how storage is split up
pub struct ShardedStore {
    primary: Arc<Store>,
    stores: HashMap<String, Arc<Store>>,
}

impl ShardedStore {
    #[allow(dead_code)]
    pub fn new(stores: HashMap<String, Arc<Store>>) -> Self {
        assert_eq!(
            1,
            stores.len(),
            "The sharded store can only handle one shard for now"
        );
        let primary = stores
            .get(PRIMARY_SHARD)
            .expect("we always have a primary store")
            .clone();
        Self { primary, stores }
    }

    // Only needed for tests
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub(crate) fn clear_storage_cache(&self) {
        for store in self.stores.values() {
            store.storage_cache.lock().unwrap().clear();
        }
    }

    fn shard(&self, id: &SubgraphDeploymentId) -> Result<String, StoreError> {
        let conn = self.primary.get_conn()?;
        let storage = self.primary.storage(&conn, id)?;
        Ok(storage.shard.clone())
    }

    fn store(&self, id: &SubgraphDeploymentId) -> Result<&Arc<Store>, StoreError> {
        let shard = self.shard(id)?;
        self.stores
            .get(&shard)
            .ok_or(StoreError::UnknownShard(shard))
    }

    fn create_deployment_internal(
        &self,
        name: SubgraphName,
        shard: String,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        mode: SubgraphVersionSwitchingMode,
        // replace == true is only used in tests; for non-test code, it must
        // be 'false'
        replace: bool,
    ) -> Result<(), StoreError> {
        #[cfg(not(debug_assertions))]
        assert!(!replace);

        let deployment_store = self
            .stores
            .get(&shard)
            .ok_or_else(|| StoreError::UnknownShard(shard.clone()))?;
        let econn = deployment_store.get_entity_conn(&*SUBGRAPHS_ID, ReplicaId::Main)?;
        let mut event = econn.transaction(|| -> Result<_, StoreError> {
            let exists = deployment::exists(&econn.conn, &schema.id)?;
            let event = if replace || !exists {
                let ops = deployment.create_operations(&schema.id);
                deployment_store.apply_metadata_operations_with_conn(&econn, ops)?
            } else {
                StoreEvent::new(vec![])
            };

            if !exists {
                econn.create_schema(shard, schema)?;
            }

            Ok(event)
        })?;

        let exists_and_synced = |id: &SubgraphDeploymentId| {
            let conn = self.store(&id)?.get_conn()?;
            deployment::exists_and_synced(&conn, id.as_str())
        };

        let conn = self.primary.get_conn()?;
        conn.transaction(|| -> Result<_, StoreError> {
            // Create subgraph, subgraph version, and assignment
            let changes = primary::create_subgraph_version(
                &conn,
                name,
                &schema.id,
                node_id,
                mode,
                exists_and_synced,
            )?;
            event.changes.extend(changes);
            self.send_store_event_with_conn(&conn, &event)?;
            Ok(())
        })
    }

    // Only for tests to simplify their handling of test fixtures, so that
    // tests can reset the block pointer of a subgraph by recreating it
    #[cfg(debug_assertions)]
    pub fn create_deployment_replace(
        &self,
        name: SubgraphName,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        mode: SubgraphVersionSwitchingMode,
    ) -> Result<(), StoreError> {
        // This works because we only allow one shard for now
        self.create_deployment_internal(
            name,
            PRIMARY_SHARD.to_string(),
            schema,
            deployment,
            node_id,
            mode,
            true,
        )
    }

    fn send_store_event_with_conn(
        &self,
        conn: &PgConnection,
        event: &StoreEvent,
    ) -> Result<(), StoreError> {
        let v = serde_json::to_value(event)?;
        #[cfg(debug_assertions)]
        {
            if *EVENT_TAP_ENABLED.lock().unwrap() {
                EVENT_TAP.lock().unwrap().push(event.clone());
            }
        }
        JsonNotification::send("store_events", &v, &conn)
    }

    pub(crate) fn send_store_event(&self, event: &StoreEvent) -> Result<(), StoreError> {
        let conn = self.primary.get_conn()?;
        self.send_store_event_with_conn(&conn, event)
    }
}

impl StoreTrait for ShardedStore {
    fn block_ptr(
        &self,
        id: SubgraphDeploymentId,
    ) -> Result<Option<EthereumBlockPointer>, failure::Error> {
        let store = self.store(&id)?;
        store.block_ptr(id)
    }

    fn supports_proof_of_indexing<'a>(
        self: Arc<Self>,
        id: &'a SubgraphDeploymentId,
    ) -> DynTryFuture<'a, bool> {
        let store = self.store(&id).unwrap().clone();
        store.supports_proof_of_indexing(id)
    }

    fn get_proof_of_indexing<'a>(
        self: Arc<Self>,
        id: &'a SubgraphDeploymentId,
        indexer: &'a Option<Address>,
        block_hash: H256,
    ) -> DynTryFuture<'a, Option<[u8; 32]>> {
        let store = self.store(&id).unwrap().clone();
        store.get_proof_of_indexing(id, indexer, block_hash)
    }

    fn get(&self, key: EntityKey) -> Result<Option<Entity>, QueryExecutionError> {
        let store = self.store(&key.subgraph_id)?;
        store.get(key)
    }

    fn get_many(
        &self,
        id: &SubgraphDeploymentId,
        ids_for_type: BTreeMap<&str, Vec<&str>>,
    ) -> Result<BTreeMap<String, Vec<Entity>>, StoreError> {
        let store = self.store(&id)?;
        store.get_many(id, ids_for_type)
    }

    fn find(&self, query: EntityQuery) -> Result<Vec<Entity>, QueryExecutionError> {
        let store = self.store(&query.subgraph_id)?;
        store.find(query)
    }

    fn find_one(&self, query: EntityQuery) -> Result<Option<Entity>, QueryExecutionError> {
        let store = self.store(&query.subgraph_id)?;
        store.find_one(query)
    }

    fn find_ens_name(&self, hash: &str) -> Result<Option<String>, QueryExecutionError> {
        self.primary.find_ens_name(hash)
    }

    fn transact_block_operations(
        &self,
        id: SubgraphDeploymentId,
        block_ptr_to: EthereumBlockPointer,
        mods: Vec<EntityModification>,
        stopwatch: StopwatchMetrics,
    ) -> Result<(), StoreError> {
        assert!(
            mods.in_shard(&id),
            "can only transact operations within one shard"
        );
        let store = self.store(&id)?;
        let event = store.transact_block_operations(id, block_ptr_to, mods, stopwatch)?;
        self.send_store_event(&event)
    }

    fn apply_metadata_operations(
        &self,
        target_deployment: &SubgraphDeploymentId,
        operations: Vec<MetadataOperation>,
    ) -> Result<(), StoreError> {
        assert!(
            operations.in_shard(target_deployment),
            "can only apply metadata operations for SubgraphDeployment and its subobjects"
        );

        let store = self.store(&target_deployment)?;
        let event = store.apply_metadata_operations(target_deployment, operations)?;
        self.send_store_event(&event)
    }

    fn revert_block_operations(
        &self,
        id: SubgraphDeploymentId,
        block_ptr_from: EthereumBlockPointer,
        block_ptr_to: EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        let store = self.store(&id)?;
        let event = store.revert_block_operations(id, block_ptr_from, block_ptr_to)?;
        self.send_store_event(&event)
    }

    fn subscribe(&self, entities: Vec<SubgraphEntityPair>) -> StoreEventStreamBox {
        // Subscriptions always go through the primary
        self.primary.subscribe(entities)
    }

    fn deployment_state_from_name(
        &self,
        name: SubgraphName,
    ) -> Result<DeploymentState, StoreError> {
        let conn = self.primary.get_conn()?;
        let id = conn.transaction(|| primary::current_deployment_for_subgraph(&conn, name))?;
        self.deployment_state_from_id(id)
    }

    fn deployment_state_from_id(
        &self,
        id: SubgraphDeploymentId,
    ) -> Result<DeploymentState, StoreError> {
        let store = self.store(&id)?;
        store.deployment_state_from_id(id)
    }

    fn start_subgraph_deployment(
        &self,
        logger: &Logger,
        id: &SubgraphDeploymentId,
    ) -> Result<(), StoreError> {
        let store = self.store(id)?;
        store.start_subgraph_deployment(logger, id)
    }

    fn block_number(
        &self,
        id: &SubgraphDeploymentId,
        block_hash: H256,
    ) -> Result<Option<BlockNumber>, StoreError> {
        let store = self.store(&id)?;
        store.block_number(id, block_hash)
    }

    fn query_store(
        self: Arc<Self>,
        id: &SubgraphDeploymentId,
        for_subscription: bool,
    ) -> Result<Arc<dyn QueryStore + Send + Sync>, StoreError> {
        assert!(
            !id.is_meta(),
            "a query store can only be retrieved for a concrete subgraph"
        );
        let store = self.store(&id)?.clone();
        store.query_store(id, for_subscription)
    }

    fn deployment_synced(&self, id: &SubgraphDeploymentId) -> Result<(), Error> {
        let pconn = self.primary.get_conn()?;
        let dconn = self.store(id)?.get_conn()?;
        let event = pconn.transaction(|| -> Result<_, Error> {
            let changes = primary::promote_deployment(&pconn, id)?;
            Ok(StoreEvent::new(changes))
        })?;
        dconn.transaction(|| deployment::set_synced(&dconn, id))?;
        Ok(self.send_store_event(&event)?)
    }

    fn create_subgraph_deployment(
        &self,
        name: SubgraphName,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        _network: String,
        mode: SubgraphVersionSwitchingMode,
    ) -> Result<(), StoreError> {
        // We only allow one shard (the primary) for now, so it is fine
        // to forward this to the primary store
        let shard = PRIMARY_SHARD.to_string();
        self.create_deployment_internal(name, shard, schema, deployment, node_id, mode, false)
    }

    fn create_subgraph(&self, name: SubgraphName) -> Result<String, StoreError> {
        let pconn = self.primary.get_conn()?;
        pconn.transaction(|| primary::create_subgraph(&pconn, &name))
    }

    fn remove_subgraph(&self, name: SubgraphName) -> Result<(), StoreError> {
        let pconn = self.primary.get_conn()?;
        let event = pconn.transaction(|| -> Result<_, StoreError> {
            let changes = primary::remove_subgraph(&pconn, name)?;
            Ok(StoreEvent::new(changes))
        })?;
        self.send_store_event(&event)
    }

    fn reassign_subgraph(
        &self,
        id: &SubgraphDeploymentId,
        node_id: &NodeId,
    ) -> Result<(), StoreError> {
        let pconn = self.primary.get_conn()?;
        let event = pconn.transaction(|| -> Result<_, StoreError> {
            let changes = primary::reassign_subgraph(&pconn, id, node_id)?;
            Ok(StoreEvent::new(changes))
        })?;
        self.send_store_event(&event)
    }

    fn status(&self, filter: status::Filter) -> Result<Vec<status::Info>, StoreError> {
        let conn = self.primary.get_conn()?;
        let (deployments, empty_means_all) = conn.transaction(|| -> Result<_, StoreError> {
            match filter {
                status::Filter::SubgraphName(name) => {
                    let deployments = primary::deployments_for_subgraph(&conn, name)?;
                    Ok((deployments, false))
                }
                status::Filter::SubgraphVersion(name, use_current) => {
                    let deployments = primary::subgraph_version(&conn, name, use_current)?
                        .map(|d| vec![d])
                        .unwrap_or_else(|| vec![]);
                    Ok((deployments, false))
                }
                status::Filter::Deployments(deployments) => Ok((deployments, true)),
            }
        })?;

        if deployments.is_empty() && !empty_means_all {
            return Ok(Vec::new());
        }

        // Ignore invalid subgraph ids
        let deployments: Vec<SubgraphDeploymentId> = deployments
            .iter()
            .filter_map(|d| SubgraphDeploymentId::new(d).ok())
            .collect();

        // For each deployment, find the shard it lives in
        let deployments_with_shard: Vec<(SubgraphDeploymentId, String)> = deployments
            .into_iter()
            .map(|id| self.shard(&id).map(|shard| (id, shard)))
            .collect::<Result<Vec<_>, StoreError>>()?;

        // Partition the list of deployments by shard
        let deployments_by_shard: HashMap<String, Vec<SubgraphDeploymentId>> =
            deployments_with_shard
                .into_iter()
                .fold(HashMap::new(), |mut map, (id, shard)| {
                    map.entry(shard).or_default().push(id);
                    map
                });

        // Go shard-by-shard to look up deployment statuses
        let mut infos = Vec::new();
        for (shard, ids) in deployments_by_shard.into_iter() {
            let store = self
                .stores
                .get(&shard)
                .ok_or(StoreError::UnknownShard(shard))?;
            let ids = ids.into_iter().map(|id| id.to_string()).collect();
            infos.extend(store.deployment_statuses(ids)?);
        }

        Ok(infos)
    }

    fn load_dynamic_data_sources(
        &self,
        id: &SubgraphDeploymentId,
    ) -> Result<Vec<StoredDynamicDataSource>, StoreError> {
        let store = self.store(id)?;
        store.load_dynamic_data_sources(id)
    }
}

/// Methods similar to those for SubgraphDeploymentStore
impl SubgraphDeploymentStore for ShardedStore {
    fn input_schema(&self, id: &SubgraphDeploymentId) -> Result<Arc<Schema>, Error> {
        let info = self.store(&id)?.subgraph_info(id)?;
        Ok(info.input)
    }

    fn api_schema(&self, id: &SubgraphDeploymentId) -> Result<Arc<ApiSchema>, Error> {
        let info = self.store(&id)?.subgraph_info(id)?;
        Ok(info.api)
    }

    fn network_name(&self, id: &SubgraphDeploymentId) -> Result<Option<String>, Error> {
        let info = self.store(&id)?.subgraph_info(id)?;
        Ok(info.network)
    }
}

impl EthereumCallCache for ShardedStore {
    fn get_call(
        &self,
        contract_address: Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
    ) -> Result<Option<Vec<u8>>, failure::Error> {
        self.primary.get_call(contract_address, encoded_call, block)
    }

    fn set_call(
        &self,
        contract_address: Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
        return_value: &[u8],
    ) -> Result<(), failure::Error> {
        self.primary
            .set_call(contract_address, encoded_call, block, return_value)
    }
}

trait ShardData {
    // Return `true` if this object resides in the shard for the
    // data for the given deployment
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool;
}

impl ShardData for MetadataType {
    fn in_shard(&self, _: &SubgraphDeploymentId) -> bool {
        use MetadataType::*;

        match self {
            Subgraph | SubgraphDeploymentAssignment => false,
            SubgraphDeployment
            | SubgraphManifest
            | EthereumContractDataSource
            | DynamicEthereumContractDataSource
            | EthereumContractSource
            | EthereumContractMapping
            | EthereumContractAbi
            | EthereumBlockHandlerEntity
            | EthereumBlockHandlerFilterEntity
            | EthereumCallHandlerEntity
            | EthereumContractEventHandler
            | EthereumContractDataSourceTemplate
            | EthereumContractDataSourceTemplateSource
            | SubgraphError => true,
        }
    }
}

impl ShardData for MetadataOperation {
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool {
        use MetadataOperation::*;
        match self {
            Set { entity, .. } | Remove { entity, .. } | Update { entity, .. } => {
                entity.in_shard(id)
            }
        }
    }
}

impl<T> ShardData for Vec<T>
where
    T: ShardData,
{
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool {
        self.iter().all(|op| op.in_shard(id))
    }
}

impl ShardData for EntityModification {
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool {
        let key = self.entity_key();
        let mod_id = &key.subgraph_id;

        if mod_id.is_meta() {
            // We do not flag an unknown MetadataType as an error here since
            // there are some valid types of metadata, e.g. SubgraphVersion
            // that are not reflected in the enum. We are just careful and
            // assume they are not stored in the same shard as subgraph data
            MetadataType::from_str(&key.entity_type)
                .ok()
                .map(|typ| typ.in_shard(id))
                .unwrap_or(false)
        } else {
            mod_id == id
        }
    }
}
