use std::collections::BTreeMap;

use web3::types::H256;

use crate::store::ReplicaId;
use graph::components::store::QueryStore as QueryStoreTrait;
use graph::prelude::*;

use crate::primary::Site;

pub(crate) struct QueryStore {
    site: Arc<Site>,
    replica_id: ReplicaId,
    store: Arc<crate::Store>,
    for_subscription: bool,
}

impl QueryStore {
    pub(crate) fn new(
        store: Arc<crate::Store>,
        for_subscription: bool,
        site: Arc<Site>,
        replica_id: ReplicaId,
    ) -> Self {
        QueryStore {
            site,
            replica_id,
            store,
            for_subscription,
        }
    }
}

impl QueryStoreTrait for QueryStore {
    fn find_query_values(
        &self,
        query: EntityQuery,
    ) -> Result<Vec<BTreeMap<String, graphql_parser::query::Value>>, QueryExecutionError> {
        assert_eq!(&self.site.deployment, &query.subgraph_id);
        let conn = self
            .store
            .get_entity_conn(self.site.as_ref(), self.replica_id)
            .map_err(|e| QueryExecutionError::StoreError(e.into()))?;
        self.store.execute_query(&conn, query)
    }

    fn subscribe(&self, entities: Vec<SubscriptionFilter>) -> StoreEventStreamBox {
        assert!(self.for_subscription);
        assert_eq!(self.replica_id, ReplicaId::Main);
        self.store.subscribe(entities)
    }

    /// Return true if the deployment with the given id is fully synced,
    /// and return false otherwise. Errors from the store are passed back up
    fn is_deployment_synced(&self, id: SubgraphDeploymentId) -> Result<bool, Error> {
        assert_eq!(&self.site.deployment, &id);
        Ok(self.store.exists_and_synced(&id)?)
    }

    fn block_ptr(
        &self,
        subgraph_id: SubgraphDeploymentId,
    ) -> Result<Option<EthereumBlockPointer>, Error> {
        assert_eq!(&self.site.deployment, &subgraph_id);
        self.store.block_ptr(&self.site)
    }

    fn block_number(
        &self,
        subgraph_id: &SubgraphDeploymentId,
        block_hash: H256,
    ) -> Result<Option<BlockNumber>, StoreError> {
        self.store.block_number(subgraph_id, block_hash)
    }

    fn wait_stats(&self) -> &PoolWaitStats {
        self.store.wait_stats(self.replica_id)
    }

    fn deployment_state(&self) -> Result<DeploymentState, QueryExecutionError> {
        Ok(self
            .store
            .deployment_state_from_id(self.site.deployment.clone())?)
    }

    fn api_schema(&self) -> Result<Arc<ApiSchema>, QueryExecutionError> {
        let info = self.store.subgraph_info(&self.site.deployment)?;
        Ok(info.api)
    }

    fn network_name(&self) -> Result<Option<String>, QueryExecutionError> {
        let info = self.store.subgraph_info(&self.site.deployment)?;
        Ok(info.network)
    }
}
