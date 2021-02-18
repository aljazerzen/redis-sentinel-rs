use log::trace;
use redis::{Client, ConnectionInfo, ErrorKind, IntoConnectionInfo, RedisError, RedisResult, aio::Connection};
use std::time::Duration;

const SENTINEL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Enables connecting to a cluster of redis instance configured
/// in high-availability mode and monitored by sentinel.
///
/// see: https://redis.io/topics/sentinel
#[derive(Debug, Clone)]
pub struct ClusterClient {
    /// used for querying addressed of master node(s).
    sentinel_nodes: Vec<redis::Client>,
    master_node: Option<redis::Client>,
    master_group_name: String,
}

impl ClusterClient {
    /// Connects to a redis-server in sentinel mode (used in redis clusters) and
    /// returns a client pointing to the current master. This does not
    /// actually open a connection yet but it does perform some basic
    /// checks on the URL that might make the operation fail.
    pub fn open<T: redis::IntoConnectionInfo>(
        sentinel_nodes: Vec<T>,
        master_group_name: String,
    ) -> RedisResult<Self> {
        let sentinel_nodes: RedisResult<Vec<ConnectionInfo>> = sentinel_nodes
            .into_iter()
            .map(|i| i.into_connection_info())
            .collect();

        let sentinel_nodes = sentinel_nodes?
            .into_iter()
            .map(|info| redis::Client::open(info))
            .collect::<RedisResult<Vec<Client>>>()?;

        Ok(Self {
            sentinel_nodes,
            master_node: None,
            master_group_name,
        })
    }

    async fn find_master(&mut self) -> RedisResult<Connection> {
        let mut master: Option<RedisResult<_>> = None;
        for (index, sentinel_node) in self.sentinel_nodes.iter().enumerate() {
            let res = self.find_master_using_sentinel(sentinel_node).await;
            let is_ok = res.is_ok();
            master = Some(res.map(|c| (index, c)));

            if is_ok {
                break;
            }
        }

        if master.is_none() {
            return Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "no sentinel nodes provided",
            )));
        }
        let (sentinel_index, (master_node, master_conn)) = master.unwrap()?;

        self.master_node = Some(master_node);

        // move connected node to start to minimize retries on reconnection
        if sentinel_index != 0 {
            let connected_node = self.sentinel_nodes.remove(sentinel_index);
            self.sentinel_nodes.insert(0, connected_node);
        }

        Ok(master_conn)
    }

    /// Returns master node client pointed to by the sentinel.
    /// See: https://redis.io/topics/sentinel-clients
    async fn find_master_using_sentinel(
        &self,
        sentinel_node: &Client,
    ) -> redis::RedisResult<(Client, Connection)> {
        // step 1): open connection
        let mut sentinel_conn = sentinel_node.get_connection_with_timeout(SENTINEL_TIMEOUT)?;

        // step 2): ask for master address
        let master_addr = self.ask_for_master_addr(&mut sentinel_conn)?;
        let master_node = redis::Client::open(master_addr)?;

        // step 3): verify it is actually a master
        let master_conn = Self::verify_master_node(&master_node).await?;

        Ok((master_node, master_conn))
    }

    /// Queries a sentinel node for the address of the current Redis master node.
    ///
    /// see step 2 of: https://redis.io/topics/sentinel-clients
    fn ask_for_master_addr(
        &self,
        sentinel_conn: &mut redis::Connection,
    ) -> redis::RedisResult<ConnectionInfo> {
        let (master_addr, master_port): (String, u16) = redis::cmd("SENTINEL")
            .arg("get-master-addr-by-name")
            .arg(&self.master_group_name)
            .query(sentinel_conn)?;
        let master_addr = format!("redis://{}:{}", master_addr, master_port);

        trace!("got redis addr from sentinel: {}", master_addr);
        master_addr.into_connection_info()
    }

    /// Verifies that a node is actually master node.
    ///
    /// see step 3 of: https://redis.io/topics/sentinel-clients
    async fn verify_master_node(master_node: &Client) -> redis::RedisResult<Connection> {
        let mut conn = master_node.get_async_connection().await?;

        let role: String = redis::cmd("ROLE").query_async(&mut conn).await?;

        if role != "master" {
            return Err(RedisError::from((
                ErrorKind::ResponseError,
                "sentinel pointed to master node but the node is not master",
            )));
        }

        Ok(conn)
    }

    /// Returns the current `redis::aio::Connection` or tries to reconnected to
    /// the advertised master node.
    pub async fn get_connection_async(&mut self) -> RedisResult<Connection> {
        if let Some(master_client) = &self.master_node {
            if let Ok(conn) = master_client.get_async_connection().await {
                return Ok(conn);
            }
        }
        
        self.find_master().await
    }
}
