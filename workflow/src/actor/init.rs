// Copyright (c) The Amphitheatre Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::errors::{Error, Result};
use crate::{Context, Intent, State, Task};

use amp_common::docker::{self, registry, DockerConfig};
use amp_common::resource::{Actor, ActorState};

use amp_resources::actor;
use async_trait::async_trait;
use kube::ResourceExt;
use tracing::{error, info, trace};

use super::{BuildingState, DeployingState};

pub struct InitialState;

#[async_trait]
impl State<Actor> for InitialState {
    /// Execute the logic for the initial state
    async fn handle(&self, ctx: &Context<Actor>) -> Option<Intent<Actor>> {
        trace!("Checking initial state of actor {}", ctx.object.name_any());

        // Check if InitTask should be executed
        let task = InitTask::new();
        if !task.matches(ctx) {
            return None;
        }

        let result = task.execute(ctx).await;
        if let Err(err) = &result {
            error!("Error during InitTask execution: {}", err);
        }

        result.ok().and_then(|intent| intent)
    }
}

pub struct InitTask;

#[async_trait]
impl Task<Actor> for InitTask {
    fn new() -> Self {
        InitTask
    }

    fn matches(&self, ctx: &Context<Actor>) -> bool {
        ctx.object.status.as_ref().is_some_and(|status| status.pending())
    }

    /// Execute the task logic for InitTask using shared data
    async fn execute(&self, ctx: &Context<Actor>) -> Result<Option<Intent<Actor>>> {
        let actor = &ctx.object;

        // build if actor is live or the image is not built, else skip to next state
        if actor.spec.live || !self.built(ctx).await? {
            let condition = ActorState::building();
            actor::patch_status(&ctx.k8s, &ctx.object, condition).await.map_err(Error::ResourceError)?;
            Ok(Some(Intent::State(Box::new(BuildingState))))
        } else {
            // patch the status to running
            let condition = ActorState::running(true, "AutoRun", None);
            actor::patch_status(&ctx.k8s, &ctx.object, condition).await.map_err(Error::ResourceError)?;
            Ok(Some(Intent::State(Box::new(DeployingState))))
        }
    }
}

impl InitTask {
    /// Check if the image is already built
    async fn built(&self, ctx: &Context<Actor>) -> Result<bool> {
        let image = &ctx.object.spec.image;

        let credentials = ctx.credentials.read().await;
        let config = DockerConfig::from(&credentials.registries);

        let credential = match docker::get_credential(&config, image) {
            Ok(credential) => Some(credential),
            Err(err) => {
                error!("Error handling docker configuration: {}", err);
                None
            }
        };

        if registry::exists(image, credential).await.map_err(Error::DockerRegistryError)? {
            info!("The images already exists");
            return Ok(true);
        }

        Ok(false)
    }
}
