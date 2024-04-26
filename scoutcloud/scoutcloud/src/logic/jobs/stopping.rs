#![allow(clippy::blocks_in_conditions)]

use crate::logic::{
    jobs::{global, utils::impl_get_db},
    DeployError, Deployment, GithubClient, Instance,
};
use fang::{typetag, AsyncQueueable, AsyncRunnable, FangError, Scheduled};
use scoutcloud_entity::sea_orm_active_enums::DeploymentStatusType;
use sea_orm::DatabaseConnection;
use std::time::Duration;

const DEFAULT_WORKFLOW_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_WORKFLOW_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(fang::serde::Serialize, fang::serde::Deserialize, Debug)]
#[serde(crate = "fang::serde")]
pub struct StoppingTask {
    deployment_id: i32,
    workflow_timeout: Duration,
    workflow_check_interval: Duration,
    #[cfg(test)]
    database_url: Option<String>,
}

impl_get_db!(StoppingTask);

impl StoppingTask {
    pub fn from_deployment_id(deployment_id: i32) -> Self {
        Self {
            deployment_id,
            workflow_timeout: DEFAULT_WORKFLOW_TIMEOUT,
            workflow_check_interval: DEFAULT_WORKFLOW_CHECK_INTERVAL,
            #[cfg(test)]
            database_url: None,
        }
    }
}

#[typetag::serde]
#[fang::async_trait]
impl AsyncRunnable for StoppingTask {
    #[tracing::instrument(err(Debug), skip(_client), level = "info")]
    async fn run(&self, _client: &mut dyn AsyncQueueable) -> Result<(), FangError> {
        let db = self.get_db().await;
        let github = global::get_github_client();

        let mut deployment = Deployment::get(db.as_ref(), self.deployment_id)
            .await
            .map_err(DeployError::Db)?;
        let instance = deployment
            .get_instance(db.as_ref())
            .await
            .map_err(DeployError::Db)?;

        // todo: save run_id to database and if deployment in stopping state, watch for it
        let result = match deployment.model.status {
            DeploymentStatusType::Running => {
                self.github_stop_and_wait(db.as_ref(), github.as_ref(), &instance, &mut deployment)
                    .await
            }
            DeploymentStatusType::Created
            | DeploymentStatusType::Failed
            | DeploymentStatusType::Pending
            | DeploymentStatusType::Stopped
            | DeploymentStatusType::Stopping => {
                tracing::warn!(
                    "cannot stop deployment '{}': invalid state '{:?}'",
                    self.deployment_id,
                    deployment.model.status,
                );
                return Ok(());
            }
        };

        if let Err(err) = result {
            tracing::error!("failed to stop deployment: {:?}", err);
            deployment
                .mark_as_error(db.as_ref(), format!("failed to stop deployment: {}", err))
                .await
                .map_err(DeployError::Db)?;
        };

        Ok(())
    }

    fn cron(&self) -> Option<Scheduled> {
        None
    }
}

impl StoppingTask {
    async fn github_stop_and_wait(
        &self,
        db: &DatabaseConnection,
        github: &GithubClient,
        instance: &Instance,
        deployment: &mut Deployment,
    ) -> Result<(), DeployError> {
        deployment
            .update_status(db, DeploymentStatusType::Stopping)
            .await?;
        let run = instance.cleanup_via_github(github).await?;
        github
            .wait_for_success_workflow(&run, self.workflow_timeout, self.workflow_check_interval)
            .await?;
        deployment.mark_as_finished(db).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_utils;

    #[tokio::test]
    async fn stopping_task_works() {
        let (db, _github, runner) =
            tests_utils::init::jobs_runner_test_case("stopping_task_works").await;
        let conn = db.client();

        let running_deployment_id = 1;
        let task = StoppingTask {
            deployment_id: running_deployment_id,
            workflow_timeout: Duration::from_secs(10),
            workflow_check_interval: Duration::from_secs(5),
            database_url: Some(db.db_url().to_string()),
        };
        runner.insert_task(&task).await.unwrap();
        tests_utils::db::wait_for_empty_fang_tasks(conn.clone())
            .await
            .unwrap();
        let deployment = Deployment::get(conn.as_ref(), running_deployment_id)
            .await
            .unwrap();
        assert_eq!(
            deployment.model.status,
            DeploymentStatusType::Stopped,
            "deployment is not stopped. error: {:?}",
            deployment.model.error
        );
    }
}
