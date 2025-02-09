use std::collections::HashSet;

use anyhow::Context;
use chrono::prelude::*;
use chrono_tz::Tz;
use google_calendar3::{hyper_rustls, hyper_util, yup_oauth2, CalendarHub};
use http_body_util::{combinators::BoxBody, BodyExt};
use hyper::Uri;
use minicaldav::{self, ical::Ical};

const WINDOW_RADIUS: chrono::TimeDelta = chrono::TimeDelta::days(14);

#[derive(Debug)]
struct Event {
    caldav_uid: Option<String>,
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

impl std::hash::Hash for Event {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.end.hash(state);
        self.summary.hash(state);
    }
}

const CALDAV_URI: &str =
    "https://radicale.sinclair.pipsimon.com/simons/6dacfbaf-8788-1a58-6888-64e264e49426/";
const GOOGLE_CALENDAR_ID: &str = "david.simon@color.com";

impl Event {
    fn to_ical(&self) -> Ical {
        let mut ical = Ical::new("VEVENT".to_string());
        ical.properties
            .push(minicaldav::ical::Property::new("SUMMARY", &self.summary));
        ical.properties.push(minicaldav::ical::Property::new(
            "DTSTART",
            &self.start.format("%Y%m%dT%H%M%SZ").to_string(),
        ));
        ical.properties.push(minicaldav::ical::Property::new(
            "DTEND",
            &self.end.format("%Y%m%dT%H%M%SZ").to_string(),
        ));
        ical
    }
}

type HyperClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    BoxBody<bytes::Bytes, hyper::Error>,
>;

fn parse_ical_datetime(property: &minicaldav::ical::Property) -> anyhow::Result<DateTime<Utc>> {
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
) -> anyhow::Result<&'a minicaldav::ical::Property> {
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

async fn fetch_caldav_events(client: &HyperClient) -> anyhow::Result<Vec<Event>> {
    let uri = CALDAV_URI.parse::<Uri>()?;
    let result = client.get(uri).await.unwrap();
    let data = String::from_utf8(result.into_body().collect().await?.to_bytes().into())?;
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
                Ok::<Event, anyhow::Error>(Event {
                    caldav_uid: Some(get_ical_property(ical_event, "UID")?.value.clone()),
                    start: parse_ical_datetime(get_ical_property(ical_event, "DTSTART")?)?,
                    end: parse_ical_datetime(get_ical_property(ical_event, "DTEND")?)?,
                    summary: get_ical_property(ical_event, "SUMMARY")?.value.clone(),
                })
            })()
            .with_context(|| {
                format!(
                    "Failed processing iCal event ({})",
                    describe_ical_event(ical_event)
                )
            })
        })
        .filter_map(|result: anyhow::Result<Event>| {
            if let Err(e) = &result {
                eprintln!("Skipping event: {:#}", e);
            }
            result.ok()
        })
        .collect())
}

async fn fetch_google_events(client: &HyperClient) -> anyhow::Result<Vec<Event>> {
    let now = chrono::Utc::now();

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
    let hub = CalendarHub::new(client.clone(), auth);

    let result = hub
        .events()
        .list(GOOGLE_CALENDAR_ID)
        .add_event_types("default")
        .max_results(2500)
        .single_events(true)
        .order_by("startTime")
        .max_attendees(1)
        .time_min(now - WINDOW_RADIUS)
        .time_max(now + WINDOW_RADIUS)
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
                caldav_uid: None,
                start: google_event.start.as_ref()?.date_time?,
                end: google_event.end.as_ref()?.date_time?,
                summary: google_event.summary.as_ref()?.clone(),
            })
        })
        .collect();

    Ok(events)
}

fn find_diff<'a>(current: &'a [Event], target: &'a [Event]) -> (Vec<&'a Event>, Vec<&'a Event>) {
    let current_set: HashSet<&Event> = current.iter().collect();
    let target_set: HashSet<&Event> = target.iter().collect();

    let mut to_delete = Vec::new();
    let mut to_create = Vec::new();

    for event in current {
        if !target_set.contains(event) {
            to_delete.push(event);
        }
    }

    for event in target {
        if !current_set.contains(event) {
            to_create.push(event);
        }
    }

    (to_delete, to_create)
}

async fn create_caldav_event(client: &HyperClient, event: &Event) -> anyhow::Result<()> {
    let uri = CALDAV_URI.parse::<Uri>()?;
    let request = hyper::Request::builder()
        .method(hyper::Method::PUT)
        .uri(uri)
        .body(BoxBody::new(
            http_body_util::Full::new(event.to_ical().serialize().into())
                .map_err(|never| match never {}),
        ))?;
    let result = client.request(request).await?;
    println!("{:?}", &result);
    let data = String::from_utf8(result.into_body().collect().await?.to_bytes().into())?;
    println!("{:?}", &data);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .build(),
        );

    let caldav_events = fetch_caldav_events(&client).await?;

    let google_events = fetch_google_events(&client).await?;

    let (to_delete, to_create) = find_diff(&caldav_events, &google_events);

    // for event in to_create {
    //     println!("{:?}", event.to_ical());
    //     create_caldav_event(&client, event).await?;
    // }

    Ok(())
}
