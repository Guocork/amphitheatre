// Copyright 2022 The Amphitheatre Authors.
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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::ObjectReference;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};

use super::Ctx;
use crate::resources::crds::{ActorSpec, Partner, Playbook, PlaybookState};
use crate::resources::error::{Error, Result};
use crate::resources::event::trace;
use crate::resources::secret::{self, Credential, Kind};
use crate::resources::{actor, namespace, playbook, service_account};

/// The reconciler that will be called when either object change
pub async fn reconcile(playbook: Arc<Playbook>, ctx: Arc<Ctx>) -> Result<Action> {
    tracing::info!("Reconciling Playbook \"{}\"", playbook.name_any());
    if playbook.spec.actors.is_empty() {
        return Err(Error::EmptyActorsError);
    }

    let api: Api<Playbook> = Api::all(ctx.client.clone());
    let finalizer_name = "playbooks.amphitheatre.app/finalizer";

    // Reconcile the playbook custom resource.
    finalizer(&api, finalizer_name, playbook, |event| async {
        match event {
            FinalizerEvent::Apply(playbook) => playbook.reconcile(ctx.clone()).await,
            FinalizerEvent::Cleanup(playbook) => playbook.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}
/// an error handler that will be called when the reconciler fails with access to both the
/// object that caused the failure and the actual error
pub fn error_policy(playbook: Arc<Playbook>, error: &Error, ctx: Arc<Ctx>) -> Action {
    tracing::error!("reconcile failed: {:?}", error);
    Action::requeue(Duration::from_secs(60))
}

impl Playbook {
    pub async fn reconcile(&self, ctx: Arc<Ctx>) -> Result<Action> {
        if let Some(ref status) = self.status {
            if status.pending() {
                self.init(ctx).await?
            } else if status.solving() {
                self.solve(ctx).await?
            } else if status.running() {
                self.run(ctx).await?
            }
        }

        // If no events were received, check back every 2 minutes
        Ok(Action::requeue(Duration::from_secs(2 * 60)))
    }

    /// Init create namespace, credentials and service accounts
    async fn init(&self, ctx: Arc<Ctx>) -> Result<()> {
        let namespace = &self.spec.namespace;
        let recorder = ctx.recorder(self.reference());

        // Create namespace for this playbook
        namespace::create(ctx.client.clone(), self).await?;
        trace(&recorder, "Created namespace for this playbook").await?;

        // Docker registry Credential
        let credential = Credential::basic(
            Kind::Image,
            "harbor.amp-system.svc.cluster.local".into(),
            "admin".into(),
            "Harbor12345".into(),
        );

        trace(&recorder, "Creating Secret for Docker Registry Credential").await?;
        secret::create(ctx.client.clone(), namespace.clone(), &credential).await?;

        // Patch this credential to default service account
        trace(&recorder, "Patch the credential to default service account").await?;
        service_account::patch(
            ctx.client.clone(),
            namespace,
            "default",
            &credential,
            true,
            true,
        )
        .await?;

        trace(&recorder, "Init successfully, Let's begin solve, now!").await?;
        playbook::patch_status(ctx.client.clone(), self, PlaybookState::solving()).await?;

        Ok(())
    }

    async fn solve(&self, ctx: Arc<Ctx>) -> Result<()> {
        let recorder = ctx.recorder(self.reference());

        let exists: HashSet<String> = self.spec.actors.iter().map(|actor| actor.url()).collect();

        let mut fetches: HashSet<Partner> = HashSet::new();
        for actor in &self.spec.actors {
            if let Some(partners) = &actor.partners {
                for partner in partners {
                    if exists.contains(&partner.url()) {
                        continue;
                    }
                    fetches.insert(partner.clone());
                }
            }
        }

        tracing::debug!("Existing repos are:\n{exists:#?}\nand fetches are: {fetches:#?}");

        for partner in fetches.iter() {
            tracing::info!("fetches url: {}", partner.url());
            let actor = read_partner(partner);

            trace(&recorder, "Fetch and add the actor to this playbook").await?;
            playbook::add(ctx.client.clone(), self, actor).await?;
        }

        if fetches.is_empty() {
            trace(&recorder, "Solved successfully, Running").await?;
            playbook::patch_status(
                ctx.client.clone(),
                self,
                PlaybookState::running(true, "AutoRun", None),
            )
            .await?;
        }

        Ok(())
    }

    async fn run(&self, ctx: Arc<Ctx>) -> Result<()> {
        let recorder = ctx.recorder(self.reference());

        for spec in &self.spec.actors {
            match actor::exists(ctx.client.clone(), self, spec).await? {
                true => {
                    // Actor already exists, update it if there are new changes
                    trace(
                        &recorder,
                        format!(
                            "Actor {} already exists, update it if there are new changes",
                            spec.name
                        ),
                    )
                    .await?;
                    actor::update(ctx.client.clone(), self, spec).await?;
                }
                false => {
                    // Create a new actor
                    trace(&recorder, format!("Create new Actor: {}", spec.name)).await?;
                    actor::create(ctx.client.clone(), self, spec).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn cleanup(&self, ctx: Arc<Ctx>) -> Result<Action> {
        Ok(Action::await_change())
    }

    fn reference(&self) -> ObjectReference {
        let mut reference = self.object_ref(&());
        reference.namespace = Some(self.spec.namespace.to_string());
        reference
    }
}

fn read_partner(partner: &Partner) -> ActorSpec {
    ActorSpec {
        name: partner.name.clone(),
        description: "A simple NodeJs example app".into(),
        image: "amp-example-nodejs".into(),
        repository: partner.repository.clone(),
        reference: partner.reference.clone(),
        path: partner.path.clone(),
        commit: "285ef2bc98fb6b3db46a96b6a750fad2d0c566b5".into(),
        ..ActorSpec::default()
    }
}
