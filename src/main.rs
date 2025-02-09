use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    path::Path,
    time::Duration,
};

use anyhow::Context;
use chrono::prelude::*;
use chrono_tz::Tz;
use google_calendar3::{hyper_rustls, hyper_util, yup_oauth2, CalendarHub};
use minicaldav::{
    self,
    ical::{self, Ical},
};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use ureq;

fn get_window_radius() -> chrono::TimeDelta {
    chrono::TimeDelta::days(
        std::env::var("WINDOW_RADIUS")
            .unwrap_or_else(|_| "14".to_string())
            .parse()
            .unwrap(),
    )
}

fn get_caldav_uri() -> String {
    std::env::var("CALDAV_URI").unwrap()
}

fn get_google_calendar_id() -> String {
    std::env::var("GOOGLE_CALENDAR_ID").unwrap()
}

fn get_google_calendar_secrets_dir() -> String {
    std::env::var("GOOGLE_CALENDAR_SECRETS_DIR").unwrap_or_else(|_| ".".to_string())
}

#[derive(Debug)]
struct Event {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    summary: String,
}

impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.end == other.end && self.summary == other.summary
    }
}

impl Eq for Event {}

impl Hash for Event {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.end.hash(state);
        self.summary.hash(state);
    }
}

#[derive(Debug)]
struct EventWithCaldavUid {
    caldav_uid: String,
    event: Event,
}

impl Event {
    fn to_ical(&self, uid: &str) -> Ical {
        let mut vcalendar = Ical::new("VCALENDAR".to_string());
        let mut vevent = Ical::new("VEVENT".to_string());
        vevent.properties.push(ical::Property::new("UID", uid));
        vevent
            .properties
            .push(ical::Property::new("SUMMARY", &self.summary));
        vevent.properties.push(ical::Property::new(
            "DTSTART",
            &self.start.format("%Y%m%dT%H%M%SZ").to_string(),
        ));
        vevent.properties.push(ical::Property::new(
            "DTEND",
            &self.end.format("%Y%m%dT%H%M%SZ").to_string(),
        ));
        vcalendar.children.push(vevent);
        vcalendar
    }
}

fn parse_ical_datetime(property: &ical::Property) -> anyhow::Result<DateTime<Utc>> {
    let str = property.value.as_str();
    if str.ends_with('Z') {
        Ok(NaiveDateTime::parse_from_str(property.value.as_str(), "%Y%m%dT%H%M%SZ")?.and_utc())
    } else {
        let tz: Tz = property
            .attributes
            .get("TZID")
            .with_context(|| "Missing key TZID in ical datetime property")?
            .parse()?;
        Ok(
            NaiveDateTime::parse_from_str(property.value.as_str(), "%Y%m%dT%H%M%S")?
                .and_local_timezone(tz)
                .single()
                .with_context(|| "Ambiguous or invalid local time")?
                .to_utc(),
        )
    }
}

fn get_ical_property<'a>(
    ical: &'a Ical,
    property_name: &str,
) -> anyhow::Result<&'a ical::Property> {
    Ok(ical
        .properties
        .iter()
        .find(|p| p.name == property_name)
        .with_context(|| format!("Looking up property: {}", property_name))?)
}

fn describe_ical_event(event: &Ical) -> String {
    format!(
        "{} '{}' at {}",
        event.name,
        event
            .properties
            .iter()
            .find(|p| p.name == "SUMMARY")
            .map(|p| p.value.as_ref())
            .unwrap_or("(Unknown summary)"),
        event
            .properties
            .iter()
            .find(|p| p.name == "DTSTART")
            .map(|p| p.value.as_ref())
            .unwrap_or("(Unknown start time)")
    )
}

fn describe_event(event: &Event) -> String {
    format!("'{}' at {}", event.summary, event.start)
}

async fn fetch_caldav_events(
    agent: &ureq::Agent,
    caldav_url: &str,
) -> anyhow::Result<Vec<EventWithCaldavUid>> {
    let data = agent.get(caldav_url).call()?.into_string()?;
    let events = minicaldav::parse_ical(&data)?;
    Ok(events
        .children
        .iter()
        .filter(|item| item.name.as_str() == "VEVENT")
        .filter(
            |ical_event| match get_ical_property(ical_event, "DTSTART") {
                // We only want events that have a time component
                Ok(prop) => prop.value.as_str().contains("T"),
                Err(_) => false,
            },
        )
        .map(|ical_event| {
            (|| {
                Ok::<EventWithCaldavUid, anyhow::Error>(EventWithCaldavUid {
                    caldav_uid: get_ical_property(ical_event, "UID")?.value.clone(),
                    event: Event {
                        start: parse_ical_datetime(get_ical_property(ical_event, "DTSTART")?)?,
                        end: parse_ical_datetime(get_ical_property(ical_event, "DTEND")?)?,
                        summary: get_ical_property(ical_event, "SUMMARY")?.value.clone(),
                    },
                })
            })()
            .with_context(|| {
                format!(
                    "Failed processing iCal event ({})",
                    describe_ical_event(ical_event)
                )
            })
        })
        .filter_map(|result: anyhow::Result<EventWithCaldavUid>| {
            if let Err(e) = &result {
                eprintln!("Skipping event: {:#}", e);
            }
            result.ok()
        })
        .collect())
}

async fn fetch_google_events() -> anyhow::Result<Vec<Event>> {
    let now = chrono::Utc::now();
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .build(),
        );

    let secrets_dir = get_google_calendar_secrets_dir();

    let secret: yup_oauth2::ApplicationSecret =
        yup_oauth2::read_application_secret(Path::new(&secrets_dir).join("secret.json"))
            .await
            .unwrap();

    let auth = yup_oauth2::InstalledFlowAuthenticator::builder(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::Interactive,
    )
    .persist_tokens_to_disk(Path::new(&secrets_dir).join("tokens.json"))
    .build()
    .await
    .unwrap();
    let hub = CalendarHub::new(client.clone(), auth);
    let window_radius = get_window_radius();

    let result = hub
        .events()
        .list(&get_google_calendar_id())
        .add_event_types("default")
        .max_results(2500)
        .single_events(true)
        .order_by("startTime")
        .max_attendees(1)
        .time_min(now - window_radius)
        .time_max(now + window_radius)
        .doit()
        .await?
        .1;

    let events = result
        .items
        .with_context(|| "Calendar events should exist")?
        .iter()
        .filter_map(|google_event| {
            if google_event
                .attendees
                .iter()
                .flatten()
                .any(|attendee| attendee.response_status == Some("declined".to_string()))
            {
                return None;
            }

            Some(Event {
                start: google_event.start.as_ref()?.date_time?,
                end: google_event.end.as_ref()?.date_time?,
                summary: google_event.summary.as_ref()?.clone(),
            })
        })
        .collect();

    Ok(events)
}

fn find_diff<'a>(
    current: &'a [EventWithCaldavUid],
    target: &'a [Event],
) -> (Vec<&'a EventWithCaldavUid>, Vec<&'a Event>) {
    let current_set: HashSet<&Event> = current.iter().map(|e| &e.event).collect();
    let target_set: HashSet<&Event> = target.iter().collect();

    let mut to_delete = Vec::new();
    let mut to_create = Vec::new();

    for event_with_caldav_uid in current {
        if !target_set.contains(&event_with_caldav_uid.event) {
            to_delete.push(event_with_caldav_uid);
        }
    }

    for event in target {
        if !current_set.contains(event) {
            to_create.push(event);
        }
    }

    (to_delete, to_create)
}

async fn create_caldav_event(
    agent: &ureq::Agent,
    caldav_url: &str,
    event: &Event,
) -> anyhow::Result<()> {
    let random_uid: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    let uri = format!("{}{}.ics", caldav_url, random_uid);
    println!("Creating event {} at {}", describe_event(event), uri);

    agent
        .put(&uri)
        .send_string(&event.to_ical(&random_uid).serialize())
        .with_context(|| format!("Failed to create event {}", describe_event(event)))?;

    Ok(())
}

async fn delete_caldav_event(
    agent: &ureq::Agent,
    caldav_url: &str,
    caldav_event: &EventWithCaldavUid,
) -> anyhow::Result<()> {
    let uri = format!("{}{}.ics", caldav_url, caldav_event.caldav_uid);
    println!(
        "Deleting event {} at {}",
        describe_event(&caldav_event.event),
        uri
    );

    agent.delete(&uri).call().with_context(|| {
        format!(
            "Failed to delete event {}",
            describe_event(&caldav_event.event)
        )
    })?;

    Ok(())
}

async fn sync() -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    println!("Starting sync at {}", now);

    let agent = ureq::Agent::new();
    let caldav_url = get_caldav_uri();

    let caldav_events = fetch_caldav_events(&agent, &caldav_url).await?;
    let google_events = fetch_google_events().await?;
    let (to_delete, to_create) = find_diff(&caldav_events, &google_events);

    println!(
        "{} events to delete, {} events to create",
        to_delete.len(),
        to_create.len()
    );

    for event in to_delete {
        delete_caldav_event(&agent, &caldav_url, event).await?;
    }

    for event in to_create {
        create_caldav_event(&agent, &caldav_url, event).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        interval.tick().await;
        sync().await?;
    }
}
