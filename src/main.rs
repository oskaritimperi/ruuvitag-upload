use std::collections::HashMap;
use std::process;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rumble;
use rumble::api::{BDAddr, Central, CentralEvent, Peripheral};
use rumble::bluez::adapter::ConnectedAdapter;

use failure::Error;

use ruuvi_sensor_protocol::{ParseError, SensorValues};

use serde::{Deserialize, Serialize};
use serde_json;

use docopt;

use reqwest;

#[derive(Serialize)]
struct Measurement {
    address: String,
    // Unix timestamp.
    timestamp: u64,
    // Relative humidity, percent.
    humidity: Option<f64>,
    // Temperature, Celcius.
    temperature: Option<f64>,
    // Pressure, kPa.
    pressure: Option<f64>,
    // Battery potential, volts.
    battery_potential: Option<f64>,
}

impl Measurement {
    fn new(address: BDAddr, values: SensorValues) -> Measurement {
        Measurement {
            address: format!("{}", address),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            humidity: values.humidity.map(|x| f64::from(x) / 10000.0),
            temperature: values.temperature.map(|x| f64::from(x) / 1000.0),
            pressure: values.pressure.map(|x| f64::from(x) / 1000.0),
            battery_potential: values.battery_potential.map(|x| f64::from(x) / 1000.0),
        }
    }
}

const USAGE: &'static str = "
ruuvitag-upload

A tool for collecting a set of ruuvitag sensor measurements
and uploading them for further processing.

The measurements are formatted as JSON with the following
structure

    {
        \"<ALIAS>\": {
            \"address\": \"XX:XX:XX:XX:XX:XX\",
            \"timestamp\": <seconds since unix epoch>,
            \"humidity\": <0-100%>,
            \"pressure\": <kPa>,
            \"temperature\": <Celcius>,
            \"battery_potential\": <volts>
        },
        ...
    }

where ALIAS will either be the address of the sensor, or
an alias that you can define.

USAGE:

    ruuvitag-upload [--url=URL] <sensor>...
    ruuvitag-upload -h | --help
    ruuvitag-upload --version

ARGUMENTS:

    <sensor>...

        A sensor address and optionally a human-readable
        alias. You can either specify the address as
        XX:XX:XX:XX:XX:XX or you can attach a human-
        readable alias to the address
        XX:XX:XX:XX:XX:XX=mysensor.

OPTIONS:

    -u URL, --url=URL

        Where the measurements are uploaded to. If you don't
        specify this, the measurements are written to stdout.

    -h, --help

        Show this message.

    --version

        Show the version number.
";

#[derive(Deserialize)]
struct Args {
    arg_sensor: Vec<String>,
    flag_url: Option<String>,
}

fn parse_sensor(s: &str) -> (&str, &str) {
    let mut it = s.split('=');
    let address = it.next().unwrap();
    let alias = if let Some(s) = it.next() { s } else { address };
    (address, alias)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {}", e);
        process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    let version = format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    );

    let args: Args = docopt::Docopt::new(USAGE)
        .and_then(|d| d.help(true).version(Some(version)).deserialize())
        .unwrap_or_else(|e| e.exit());

    let sensors: HashMap<String, String> = args
        .arg_sensor
        .iter()
        .map(|x| parse_sensor(x))
        .map(|(address, alias)| (address.to_string(), alias.to_string()))
        .collect();
    let sensors = Arc::new(sensors);
    let sensors_clone = sensors.clone();

    let manager = rumble::bluez::manager::Manager::new()?;

    let mut adapter = manager.adapters()?.into_iter().nth(0).unwrap();

    adapter = manager.down(&adapter)?;
    adapter = manager.up(&adapter)?;

    let central = Arc::new(adapter.connect()?);

    central.active(false);
    central.filter_duplicates(false);

    let central_clone = central.clone();

    let measurements: Arc<Mutex<HashMap<String, Measurement>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let measurements_clone = measurements.clone();

    let event = Arc::new((Mutex::new(false), Condvar::new()));
    let event_clone = event.clone();

    central.on_event(Box::new(move |event| {
        if let Some(result) = on_event(&central_clone, event) {
            match result {
                Ok(measurement) => {
                    if let Some(alias) = sensors_clone.get(&measurement.address) {
                        let mut measurements = measurements_clone.lock().unwrap();
                        measurements.insert(alias.clone(), measurement);
                        if measurements.len() == sensors_clone.len() {
                            let &(ref lock, ref cv) = &*event_clone;
                            let mut done = lock.lock().unwrap();
                            *done = true;
                            cv.notify_one();
                        }
                    }
                }
                Err(_) => (),
            }
        }
    }));

    loop {
        central.start_scan()?;

        {
            let &(ref lock, ref cv) = &*event;
            let mut signalled = lock.lock().unwrap();
            while !*signalled {
                let result = cv.wait_timeout(signalled, Duration::from_secs(60)).unwrap();
                signalled = result.0;
                if result.1.timed_out() {
                    break;
                }
            }
        }

        central.stop_scan()?;

        {
            let &(ref lock, _) = &*event;
            let signalled = lock.lock().unwrap();
            if *signalled {
                break;
            }
        }
    }

    if let Some(url) = args.flag_url {
        let client = reqwest::Client::new();

        client
            .post(&url)
            .json(&*measurements.lock().unwrap())
            .send()?
            .error_for_status()?;
    } else {
        println!("{}", serde_json::to_string(&*measurements.lock().unwrap()).unwrap());
    }

    Ok(())
}

fn on_event(
    central: &ConnectedAdapter,
    event: CentralEvent,
) -> Option<Result<Measurement, ParseError>> {
    match event {
        CentralEvent::DeviceDiscovered(addr) => on_event_with_address(central, addr),
        CentralEvent::DeviceUpdated(addr) => on_event_with_address(central, addr),
        _ => None,
    }
}

fn on_event_with_address(
    central: &ConnectedAdapter,
    address: BDAddr,
) -> Option<Result<Measurement, ParseError>> {
    match central.peripheral(address) {
        Some(peripheral) => match to_sensor_value(peripheral) {
            Ok(values) => Some(Ok(Measurement::new(address, values))),
            Err(e) => Some(Err(e)),
        },
        None => None,
    }
}

fn to_sensor_value<T: Peripheral>(peripheral: T) -> Result<SensorValues, ParseError> {
    match peripheral.properties().manufacturer_data {
        Some(data) => from_manufacturer_data(&data),
        None => Err(ParseError::EmptyValue),
    }
}

fn from_manufacturer_data(data: &[u8]) -> Result<SensorValues, ParseError> {
    if data.len() > 2 {
        let id = u16::from(data[0]) + (u16::from(data[1]) << 8);
        SensorValues::from_manufacturer_specific_data(id, &data[2..])
    } else {
        Err(ParseError::EmptyValue)
    }
}
