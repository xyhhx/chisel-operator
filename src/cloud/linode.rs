use super::{
    cloud_init::generate_cloud_init_config, pwgen::generate_password, CloudExitNode, Provisioner,
};
use crate::ops::{ExitNode, ExitNodeProvisioner, ExitNodeStatus, EXIT_NODE_PROVISIONER_LABEL};
use async_trait::async_trait;
use base64::Engine;
use color_eyre::eyre::{anyhow, Error};
use k8s_openapi::api::core::v1::Secret;
use linode_rs::{LinodeApi, LinodeInstance};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const TOKEN_KEY: &str = "LINODE_TOKEN";
const INSTANCE_TYPE: &str = "g6-nanode-1";
const IMAGE_ID: &str = "linode/ubuntu22.04";
#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct LinodeProvisioner {
    pub auth: String,
    pub region: String,
}

impl LinodeProvisioner {
    // gets token from Secret
    pub async fn get_token(&self, secret: &Secret) -> color_eyre::Result<String> {
        let data = secret
            .data
            .clone()
            .ok_or_else(|| Error::msg("No data found in secret"))?;
        let token = data
            .get(TOKEN_KEY)
            .ok_or_else(|| Error::msg("No token found in secret"))?;

        let token = String::from_utf8(token.clone().0)?;
        Ok(token)
    }
}

#[async_trait]
impl Provisioner for LinodeProvisioner {
    async fn create_exit_node(
        &self,
        auth: Secret,
        exit_node: ExitNode,
    ) -> color_eyre::Result<ExitNodeStatus> {
        let password = generate_password(32);

        let _secret = exit_node.generate_secret(password.clone()).await?;

        let config = generate_cloud_init_config(&password, exit_node.spec.port);

        // Okay, so apparently Linode uses base64 for user_data, so let's
        // base64 encode the config

        let user_data = base64::engine::general_purpose::STANDARD.encode(config);

        let api = LinodeApi::new(self.get_token(&auth).await?);

        let provisioner = exit_node
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(EXIT_NODE_PROVISIONER_LABEL))
            .unwrap();

        let name = format!(
            "{}-{}",
            provisioner,
            exit_node.metadata.name.as_ref().unwrap()
        );

        let mut instance = api
            .create_instance(&self.region, INSTANCE_TYPE)
            .root_pass(&password)
            .label(&name)
            .user_data(&user_data)
            .tags(vec![format!("chisel-operator-provisioner:{}", provisioner)])
            .image(IMAGE_ID)
            .booted(true)
            .run_async()
            .await?;

        info!("Created instance: {:?}", instance);

        let mut instance_ip: Option<String> = None;

        while instance_ip.is_none() {
            instance = api.get_instance_async(instance.id).await?;

            debug!("Instance status: {:?}", instance.status);

            if instance.ipv4.len() > 0 {
                instance_ip = Some(instance.ipv4[0].clone());
            } else {
                warn!("Waiting for instance to get IP address");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }

        let instance_ip = instance_ip.unwrap();

        let status = ExitNodeStatus {
            ip: instance_ip,
            name: instance.label,
            provider: provisioner.to_string(),
            id: Some(instance.id.to_string()),
        };

        Ok(status)
    }

    async fn delete_exit_node(&self, auth: Secret, exit_node: ExitNode) -> color_eyre::Result<()> {
        let api = LinodeApi::new(self.get_token(&auth).await?);

        let instance_id = exit_node
            .status
            .as_ref()
            .and_then(|status| status.id.as_ref())
            .and_then(|id| id.parse::<u64>().ok());

        // okay, so Linode IDs will be u64, so let's parse it

        if let Some(instance_id) = instance_id {
            info!("Deleting Linode instance with ID {}", instance_id);
            api.delete_instance_async(instance_id).await?;
        }


        Ok(())
    }

    async fn update_exit_node(
        &self,
        auth: Secret,
        exit_node: ExitNode,
    ) -> color_eyre::Result<ExitNodeStatus> {
        let api = LinodeApi::new(self.get_token(&auth).await?);

        if let Some(status) = exit_node.status {
            let instance_id = status
                .id
                .as_ref()
                .ok_or_else(|| anyhow!("No instance ID found in status"))?
                .parse::<u64>()?;

            let instance = api.get_instance_async(instance_id).await;

            let mut status = status.clone();

            if let Some(ip) = instance?.ipv4.first() {
                status.ip = ip.to_owned();
            }

            Ok(status)
        } else {
            warn!("No instance status found, creating new instance");
            return Ok(self.create_exit_node(auth.clone(), exit_node).await?);
        }
    }
}
