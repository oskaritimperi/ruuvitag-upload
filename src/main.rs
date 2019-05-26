use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::process;
use std::sync::{mpsc::channel, Arc};
use std::time::{SystemTime, UNIX_EPOCH};

use rumble;
use rumble::api::{BDAddr, Central, CentralEvent, Peripheral};
use rumble::bluez::adapter::ConnectedAdapter;

use failure::Error;

use ruuvi_sensor_protocol::{ParseError, SensorValues};

use serde::{Deserialize, Serialize};
use serde_json;

use docopt;

use reqwest;

use directories::ProjectDirs;

#[derive(Serialize, Deserialize)]
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

const USAGE: &str = "
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

If uploading measurements fails, the measurements are
cached. The cached measurements are uploaded the next time
ruuvitag-upload is called. Cached measurements are uploaded
first, from oldest to newest. If uploading cached measurements
fails, the current measurements are again cached for next time.
This way, you won't lose any measurements. When a cached
measurement is succesfully uploaded, the cache entry will be
removed.

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

    let measurements = collect_measurements(sensors)?;

    if let Some(url) = args.flag_url {
        let result = upload_cached_measurements(&url);

        // If uploading cached measurements failed, we try to cache the latest measurements.
        if result.is_err() {
            eprintln!("error: {}", result.unwrap_err());
            cache_measurements(measurements)?;
            return Ok(());
        }

        let client = reqwest::Client::new();

        let result = match client.post(&url).json(&measurements).send() {
            Ok(response) => match response.error_for_status() {
                Ok(response) => Ok(response),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };

        // If uploading the latest measurements failed, we try to cache them for later uploading.
        if result.is_err() {
            eprintln!("error: {}", result.unwrap_err());
            cache_measurements(measurements)?;
        }
    } else {
        println!("{}", serde_json::to_string(&measurements).unwrap());
    }

    Ok(())
}

fn find_cached_measurements(cache_dir: &Path) -> Result<Vec<std::path::PathBuf>, Error> {
    let mut result = Vec::new();

    // It's OK if we don't find cached data. Other errors are not good.
    if let Err(error) = fs::metadata(cache_dir) {
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(result);
        }
        return Err(error.into());
    }

    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "json" {
                    result.push(path);
                }
            }
        }
    }

    result.sort();

    Ok(result)
}

fn upload_cached_measurements(url: &str) -> Result<(), Error> {
    let paths = find_cached_measurements(&get_cache_dir()?)?;

    let client = reqwest::Client::new();

    for path in paths {
        let file = fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let measurements: HashMap<String, Measurement> = serde_json::from_reader(reader)?;
        client
            .post(url)
            .json(&measurements)
            .send()?
            .error_for_status()?;
        fs::remove_file(&path)?;
    }

    Ok(())
}

fn get_cache_dir() -> Result<std::path::PathBuf, Error> {
    match ProjectDirs::from("dev", "otimperi", "ruuvitag-upload") {
        None => Err(failure::format_err!("failed to get cache dir location")),
        Some(dir) => Ok(dir.data_dir().to_path_buf()),
    }
}

fn cache_measurements(measurements: HashMap<String, Measurement>) -> Result<(), Error> {
    let mut path = get_cache_dir()?;

    path.push(format!(
        "{}.json",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    ));

    eprintln!("caching measurements to {}", path.display());

    std::fs::create_dir_all(path.parent().unwrap())?;

    let mut file = std::fs::File::create(path)?;

    let json = serde_json::to_string(&measurements)?;

    file.write_all(&json.into_bytes())?;

    Ok(())
}

fn collect_measurements(
    sensors: HashMap<String, String>,
) -> Result<HashMap<String, Measurement>, Error> {
    let manager = rumble::bluez::manager::Manager::new()?;

    let mut adapter = manager.adapters()?.into_iter().nth(0).unwrap();

    adapter = manager.down(&adapter)?;
    adapter = manager.up(&adapter)?;

    let central = Arc::new(adapter.connect()?);

    let central_clone = central.clone();

    let (meas_tx, meas_rx) = channel();

    central.on_event(Box::new(move |event| {
        if let Some(result) = on_event(&central_clone, event) {
            if let Ok(measurement) = result {
                let _ = meas_tx.send(measurement);
            }
        }
    }));

    central.start_scan()?;

    let mut measurements = HashMap::new();

    loop {
        let measurement = meas_rx.recv()?;
        if let Some(alias) = sensors.get(&measurement.address) {
            measurements.insert(alias.clone(), measurement);
            if measurements.len() == sensors.len() {
                break;
            }
        }
    }

    central.stop_scan()?;

    Ok(measurements)
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn test_find_cached_measurements() {
        let test_dir = assert_fs::TempDir::new().unwrap();

        test_dir.child("1236.json").touch().unwrap();
        test_dir.child("1233.cmd").touch().unwrap();
        test_dir.child("1234.json").touch().unwrap();
        test_dir.child("1235.json").touch().unwrap();
        test_dir.child("1238.md").touch().unwrap();
        test_dir.child("1237.txt").touch().unwrap();

        let files: Vec<String> = find_cached_measurements(test_dir.path())
            .unwrap()
            .iter()
            .filter_map(|path| path.file_name())
            .map(|file_name| file_name.to_string_lossy().into_owned())
            .collect();

        assert_eq!(files, vec!["1234.json", "1235.json", "1236.json"]);
    }
}
