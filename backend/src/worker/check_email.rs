// Reacher - Email Verification
// Copyright (C) 2018-2023 Reacher

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published
// by the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.

// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use super::response::save_to_db;
use crate::config::BackendConfig;
use crate::worker::response::send_single_shot_reply;
use check_if_email_exists::{
	check_email, CheckEmailInput, CheckEmailOutput, Reachable, LOG_TARGET,
};
use core::time;
use lapin::message::Delivery;
use lapin::{options::*, Channel};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};
use warp::http::StatusCode;

#[derive(Debug, Deserialize, Serialize)]
pub struct CheckEmailTask {
	pub input: CheckEmailInput,
	// If the task is a part of a job, then this field will be set.
	pub job_id: Option<i32>,
	pub webhook: Option<TaskWebhook>,
}

impl CheckEmailTask {
	/// Returns true if the task is a single-shot email verification via the
	/// /v1/check_email, endpoint, i.e. not a part of a bulk verification job.
	pub fn is_single_shot(&self) -> bool {
		self.job_id.is_none()
	}
}

/// The errors that can occur when processing a task.
#[derive(Debug, Error)]
pub enum TaskError {
	/// The worker is at full capacity and cannot accept more tasks. Note that
	/// this error only occurs for single-shot tasks, and not for bulk
	/// verification, as for bulk verification tasks the task will simply stay
	/// in the queue until one worker is ready to process it.
	#[error("Worker at full capacity, wait {0:?}")]
	Throttle(time::Duration),
	#[error("Lapin error: {0}")]
	Lapin(lapin::Error),
	#[error("Reqwest error during webhook: {0}")]
	Reqwest(reqwest::Error),
}

impl TaskError {
	/// Returns the status code that should be returned to the client.
	pub fn status_code(&self) -> StatusCode {
		match self {
			Self::Throttle(_) => StatusCode::TOO_MANY_REQUESTS,
			Self::Lapin(_) => StatusCode::INTERNAL_SERVER_ERROR,
			Self::Reqwest(_) => StatusCode::INTERNAL_SERVER_ERROR,
		}
	}
}

impl From<lapin::Error> for TaskError {
	fn from(err: lapin::Error) -> Self {
		Self::Lapin(err)
	}
}

impl From<reqwest::Error> for TaskError {
	fn from(err: reqwest::Error) -> Self {
		Self::Reqwest(err)
	}
}

impl Serialize for TaskError {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(&self.to_string())
	}
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct TaskWebhook {
	pub on_each_email: Option<Webhook>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Webhook {
	pub url: String,
	pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct WebhookOutput<'a> {
	result: &'a CheckEmailOutput,
	extra: &'a Option<serde_json::Value>,
}

/// Processes the check email task asynchronously.
pub(crate) async fn do_check_email_work(
	payload: &CheckEmailTask,
	delivery: Delivery,
	channel: Arc<Channel>,
	config: Arc<BackendConfig>,
) -> Result<(), anyhow::Error> {
	let worker_output = inner_check_email(payload, Arc::clone(&config)).await;

	match (&worker_output, delivery.redelivered) {
		(Ok(output), false) if output.is_reachable == Reachable::Unknown => {
			// If is_reachable is unknown, then we requeue the message, but only once.
			// We might want to add a requeue counter in the future, see:
			// https://stackoverflow.com/questions/25226080/rabbitmq-how-to-requeue-message-with-counter
			delivery
				.reject(BasicRejectOptions { requeue: true })
				.await?;
			info!(target: LOG_TARGET, email=?&payload.input.to_email, is_reachable=?Reachable::Unknown, "Requeued message");
		}
		(Err(e), false) => {
			// Same as above, if processing the message failed, we requeue it.
			delivery
				.reject(BasicRejectOptions { requeue: true })
				.await?;
			info!(target: LOG_TARGET, email=?&payload.input.to_email, err=?e, "Requeued message");
		}
		_ => {
			// This is the happy path. We acknowledge the message and:
			// - If it's a single-shot email verification, we send a reply to the client.
			// - If it's a bulk verification, we save the result to the database.
			delivery.ack(BasicAckOptions::default()).await?;

			if payload.is_single_shot() {
				send_single_shot_reply(channel, &delivery, &worker_output).await?;
			} else {
				save_to_db(
					&config.backend_name,
					config.get_pg_pool(),
					payload,
					&worker_output,
				)
				.await?;
			}

			info!(target: LOG_TARGET,
				email=payload.input.to_email,
				worker_output=?worker_output.map(|o| o.is_reachable),
				job_id=?payload.job_id,
				"Done check",
			);
		}
	}

	Ok(())
}

async fn inner_check_email(
	payload: &CheckEmailTask,
	config: Arc<BackendConfig>,
) -> Result<CheckEmailOutput, TaskError> {
	let output = check_email(&payload.input, &config.get_reacher_config()).await;

	// Check if we have a webhook to send the output to.
	if let Some(TaskWebhook {
		on_each_email: Some(webhook),
	}) = &payload.webhook
	{
		let webhook_output = WebhookOutput {
			result: &output,
			extra: &webhook.extra,
		};

		let client = reqwest::Client::new();
		let res = client
			.post(&webhook.url)
			.json(&webhook_output)
			.header(
				"x-reacher-secret",
				std::env::var("RCH_HEADER_SECRET").unwrap_or_default(),
			)
			.send()
			.await?
			.text()
			.await?;
		debug!(target: LOG_TARGET, email=?webhook_output.result.input,res=?res, "Received webhook response");
	}

	Ok(output)
}
