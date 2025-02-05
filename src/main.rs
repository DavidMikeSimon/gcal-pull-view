use anyhow::Context;
use chrono::prelude::*;
use google_calendar3::{api::EventDateTime, hyper_rustls, hyper_util, yup_oauth2, CalendarHub};

const DAYS_WINDOW: u64 = 7;

#[derive(Debug)]
struct Event {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    summary: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let secret: yup_oauth2::ApplicationSecret = yup_oauth2::read_application_secret("secret.json")
        .await
        .unwrap();

    let auth = yup_oauth2::InstalledFlowAuthenticator::builder(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::Interactive,
    )
    .persist_tokens_to_disk("tokens.json")
    .build()
    .await
    .unwrap();

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .build(),
        );
    let hub = CalendarHub::new(client, auth);

    let now = chrono::Utc::now();

    let result = hub
        .events()
        .list("david.simon@color.com")
        .add_event_types("default")
        .max_results(2500)
        .single_events(true)
        .order_by("startTime")
        .max_attendees(1)
        .time_min(
            now.checked_sub_days(chrono::Days::new(DAYS_WINDOW))
                .context("Subtracting days")?,
        )
        .time_max(
            now.checked_add_days(chrono::Days::new(DAYS_WINDOW))
                .context("Adding days")?,
        )
        .doit()
        .await?
        .1;

    let events = result
        .items
        .context("Calendar events should exist")?
        .iter()
        .flat_map(|event| match event {
            google_calendar3::api::Event {
                start:
                    Some(EventDateTime {
                        date_time: Some(start),
                        ..
                    }),
                end:
                    Some(EventDateTime {
                        date_time: Some(end),
                        ..
                    }),
                summary: Some(summary),
                attendees,
                ..
            } => {
                if let Some(attendees) = attendees {
                    if let Some(attendee) = attendees.get(0) {
                        if attendee.response_status == Some("declined".to_string()) {
                            return None;
                        }
                    }
                }
                Some(Event {
                    start: start.clone(),
                    end: end.clone(),
                    summary: summary.clone(),
                })
            }
            _ => None,
        })
        .collect::<Vec<Event>>();

    events.iter().for_each(|event| {
        println!("Event: {:?}", event);
    });

    Ok(())
}
