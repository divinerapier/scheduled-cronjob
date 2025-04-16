use std::ops::Deref;

use crate::ScheduledCronJobStatus;
use crate::crd::{ScheduledCronJob, ScheduledCronJobPhase};
use chrono::Utc;
use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::{Event, EventSeries};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};
use kube::ResourceExt;
use kube::api::{DeleteParams, PostParams};
use kube::core::Resource as KubeResource;
use kube::core::object::HasStatus;
use kube::{Api, Client, Error as KubeError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json;

pub struct Context {
    client: Client,
}

impl Context {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub async fn get<K>(&self, namespace: &str, name: &str) -> Result<K, crate::Error>
    where
        K: KubeResource<Scope = NamespaceResourceScope>,
        K: KubeResource,
        K: Clone + DeserializeOwned + std::fmt::Debug,
        K::DynamicType: Default,
    {
        let api = Api::<K>::namespaced(self.client.clone(), namespace);
        match api.get(name).await {
            Ok(object) => Ok(object),
            Err(KubeError::Api(e)) if e.code == 404 => Err(crate::Error::NotFound),
            Err(e) => Err(crate::Error::Kube(e)),
        }
    }

    pub async fn create<K>(&self, namespace: &str, object: &K) -> Result<K, crate::Error>
    where
        K: KubeResource<Scope = NamespaceResourceScope>,
        K: KubeResource,
        K: Clone + DeserializeOwned + Serialize + std::fmt::Debug,
        K::DynamicType: Default,
    {
        let api = Api::<K>::namespaced(self.client.clone(), namespace);
        match api.create(&PostParams::default(), object).await {
            Ok(object) => Ok(object),
            Err(e) => Err(crate::Error::Kube(e)),
        }
    }

    pub async fn delete<K>(&self, namespace: &str, name: &str) -> Result<(), crate::Error>
    where
        K: KubeResource<Scope = NamespaceResourceScope>,
        K: KubeResource,
        K: Clone + DeserializeOwned + Serialize + std::fmt::Debug,
        K::DynamicType: Default,
    {
        let api = Api::<K>::namespaced(self.client.clone(), namespace);
        if let Err(e) = api.delete(name, &DeleteParams::foreground()).await {
            match e {
                KubeError::Api(e) if e.code == 404 => return Ok(()),
                _ => return Err(crate::Error::Kube(e)),
            }
        }
        Ok(())
    }

    pub async fn create_cronjob(
        &self,
        namespace: &str,
        object: &CronJob,
    ) -> Result<CronJob, crate::Error> {
        self.create(namespace, object).await
    }

    pub async fn update(
        &self,
        resource: &ScheduledCronJob,
        status: ScheduledCronJobPhase,
        event_type: &str,
        message: &str,
    ) -> Result<(), crate::Error> {
        tracing::info!(
            name = resource.name_any(),
            namespace = resource.namespace().unwrap_or_default(),
            status = status.as_str(),
            message = message,
            "Updating status for scheduled cronjob",
        );
        self.create_event(resource, event_type, status.as_str(), message)
            .await
            .unwrap();
        self.update_status(resource, status, message).await.unwrap();
        Ok(())
    }

    pub async fn update_status(
        &self,
        resource: &ScheduledCronJob,
        status: ScheduledCronJobPhase,
        message: &str,
    ) -> Result<(), crate::Error> {
        let namespace = resource.namespace().unwrap_or_default();
        let name = resource.name_any();
        let api = Api::<ScheduledCronJob>::namespaced(self.client.clone(), &namespace);

        let mut resource = match api.get(&name).await {
            Ok(resource) => resource,
            Err(KubeError::Api(e)) if e.code == 404 => return Ok(()),
            Err(e) => return Err(crate::Error::Kube(e)),
        };
        resource.status = Some(ScheduledCronJobStatus {
            phase: status,
            message: Some(message.to_string()),
            last_update_time: Some(Utc::now().to_rfc3339()),
        });

        assert_eq!(resource.status().unwrap().phase, status);
        assert_eq!(
            resource.status().unwrap().message,
            Some(message.to_string())
        );

        let bytes = serde_json::to_vec(&resource)?;
        api.replace_status(&name, &PostParams::default(), bytes)
            .await?;
        Ok(())
    }

    pub async fn create_event(
        &self,
        resource: &ScheduledCronJob,
        event_type: &str,
        reason: &str,
        message: &str,
    ) -> Result<(), crate::Error> {
        let namespace = resource.namespace().unwrap_or_default();
        let name = resource.name_any();
        let api = Api::<Event>::namespaced(self.client.clone(), &namespace);
        let now = Utc::now();

        let api_version = ScheduledCronJob::api_version(&());

        assert_eq!(api_version, "batch.divinerapier.io/v1alpha1");

        let event = Event {
            metadata: ObjectMeta {
                name: Some(format!("{}-{}", name, now.timestamp())),
                namespace: Some(namespace.clone()),
                ..Default::default()
            },
            action: Some("Reconciling".to_string()),
            count: Some(1),
            event_time: Some(MicroTime(now)),
            first_timestamp: Some(Time(now)),
            involved_object: k8s_openapi::api::core::v1::ObjectReference {
                kind: Some("ScheduledCronJob".to_string()),
                namespace: Some(namespace),
                name: Some(name),
                api_version: Some(api_version.to_string()),
                uid: resource.metadata.uid.clone(),
                ..Default::default()
            },
            last_timestamp: Some(Time(now)),
            message: Some(message.to_string()),
            reason: Some(reason.to_string()),
            reporting_component: Some("scheduled-cronjob".to_string()),
            reporting_instance: Some("scheduled-cronjob-controller".to_string()),
            type_: Some(event_type.to_string()),
            series: Some(EventSeries {
                count: Some(1),
                last_observed_time: Some(MicroTime(now)),
                ..Default::default()
            }),
            source: Some(k8s_openapi::api::core::v1::EventSource {
                component: Some("scheduled-cronjob".to_string()),
                ..Default::default()
            }),
            related: None,
        };

        match api.create(&PostParams::default(), &event).await {
            Ok(_) => Ok(()),
            Err(KubeError::Api(e)) if e.code == 409 => Ok(()),
            Err(e) => Err(crate::Error::Kube(e)),
        }
    }
}

impl Deref for Context {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}
