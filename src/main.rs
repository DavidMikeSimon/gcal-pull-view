use chrono::{prelude::*, Duration};
use icalendar::{Calendar, CalendarComponent, Component, DatePerhapsTime};
use ureq;

#[derive(Debug)]
struct Event {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    summary: String,
}

// const MAX_DISTANCE: Duration = Duration::days(14);
const MAX_DISTANCE: Duration = Duration::days(5);
const TARGET_EMAIL: &'static str = "david.simon@color.com";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // let url =
    //     std::env::var("GOOGLE_CALENDAR_ICAL_URL").expect("GOOGLE_CALENDAR_ICAL_URL must be set");
    // let body = ureq::get(url).call()?.body_mut().read_to_string()?;

    let body = std::fs::read_to_string("basic.ics")?;

    let now = Utc::now();

    let calendar: Calendar = body.parse()?;

    let mut events: Vec<Event> = calendar
        .components
        .iter()
        .filter_map(|component| {
            let event = component.as_event()?;

            if !event.get_summary()?.to_string().contains("Geena / David") {
                return None;
            }

            println!("{:?}", event);

            let start = match event.get_start() {
                Some(DatePerhapsTime::DateTime(start)) => start.try_into_utc()?,
                _ => return None,
            };

            let end = match event.get_end() {
                Some(DatePerhapsTime::DateTime(end)) => end.try_into_utc()?,
                _ => return None,
            };

            // if start.signed_duration_since(now).abs() > MAX_DISTANCE {
            //     return None;
            // }

            let attendee_match = event
                .multi_properties()
                .get("ATTENDEE")
                .unwrap_or(&vec![])
                .iter()
                .any(|attendee| {
                    let email = attendee.params().get("CN");
                    let status = attendee.params().get("PARTSTAT");
                    match (email, status) {
                        (Some(email), Some(status)) => {
                            email.value() == TARGET_EMAIL && status.value() == "ACCEPTED"
                        }
                        _ => false,
                    }
                });

            // if !attendee_match {
            //     return None;
            // }

            return Some(Event {
                start,
                end,
                summary: event.get_summary()?.to_string(),
            });
        })
        .collect();

    events.sort_by_key(|event| event.start);

    events.iter().for_each(|event| {
        println!("{:?}", event);
    });

    return Ok(());
}
