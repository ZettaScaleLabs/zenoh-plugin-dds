//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use std::{
    str::FromStr,
    time::{Duration, SystemTime},
};

use async_liveliness_monitor::LivelinessMonitor;
use clap::{App, Arg};
use zenoh::{
    config::Config,
    internal::{plugins::PluginsManager, runtime::RuntimeBuilder},
    session::ZenohId,
};
use zenoh_config::ModeDependentValue;
use zenoh_plugin_dds::DDSPlugin;
use zenoh_plugin_trait::Plugin;

lazy_static::lazy_static!(
    pub static ref DEFAULT_DOMAIN_STR: String = zenoh_plugin_dds::config::DEFAULT_DOMAIN.to_string();
);

macro_rules! insert_json5 {
    ($config: expr, $args: expr, $key: expr, if $name: expr) => {
        if $args.occurrences_of($name) > 0 {
            $config.insert_json5($key, "true").unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, if $name: expr, $($t: tt)*) => {
        if $args.occurrences_of($name) > 0 {
            $config.insert_json5($key, &serde_json::to_string(&$args.value_of($name).unwrap()$($t)*).unwrap()).unwrap();
        }
    };
    ($config: expr, $args: expr, $key: expr, for $name: expr, $($t: tt)*) => {
        if let Some(value) = $args.values_of($name) {
            $config.insert_json5($key, &serde_json::to_string(&value$($t)*).unwrap()).unwrap();
        }
    };
}

fn parse_args() -> (Config, Option<f32>) {
    let mut app = App::new("zenoh bridge for DDS")
        .version(DDSPlugin::PLUGIN_VERSION)
        .long_version(DDSPlugin::PLUGIN_LONG_VERSION)
        //
        // zenoh related arguments:
        //
        .arg(Arg::from_usage(
r"-i, --id=[HEX_STRING] \
'The identifier (as an hexadecimal string, with odd number of chars - e.g.: 0A0B23...) that zenohd must use.
WARNING: this identifier must be unique in the system and must be 16 bytes maximum (32 chars)!
If not set, a random UUIDv4 will be used.'",
            ))
        .arg(Arg::from_usage(
r#"-m, --mode=[MODE]  'The zenoh session mode.'"#)
            .possible_values(["peer", "client"])
            .default_value("peer")
        )
        .arg(Arg::from_usage(
r"-c, --config=[FILE] \
'The configuration file. Currently, this file must be a valid JSON5 file.'",
            ))
        .arg(Arg::from_usage(
r"-l, --listen=[ENDPOINT]... \
'A locator on which this router will listen for incoming sessions.
Repeat this option to open several listeners.'",
                ),
            )
        .arg(Arg::from_usage(
r"-e, --connect=[ENDPOINT]... \
'A peer locator this router will try to connect to.
Repeat this option to connect to several peers.'",
            ))
        .arg(Arg::from_usage(
r"--no-multicast-scouting \
'By default the zenoh bridge listens and replies to UDP multicast scouting messages for being discovered by peers and routers.
This option disables this feature.'"
        ))
        .arg(Arg::from_usage(
r"--rest-http-port=[PORT | IP:PORT] \
'Configures HTTP interface for the REST API (disabled by default, setting this option enables it). Accepted values:'
  - a port number
  - a string with format `<local_ip>:<port_number>` (to bind the HTTP server to a specific interface)."
        ))
        //
        // DDS related arguments:
        //
        .arg(Arg::from_usage(
r#"-s, --scope=[String]   'A string added as prefix to all routed DDS topics when mapped to a zenoh resource. This should be used to avoid conflicts when several distinct DDS systems using the same topics names are routed via zenoh'"#
        ))
        .arg(Arg::from_usage(
r#"-d, --domain=[ID]   'The DDS Domain ID. The default value is "$ROS_DOMAIN_ID" if defined, or "0" otherwise.'"#)
            .default_value(&DEFAULT_DOMAIN_STR)
        )
        .arg(Arg::from_usage(
r#"--dds-localhost-only \
'Configure CycloneDDS to use only the localhost interface. If not set, CycloneDDS will pick the interface defined in "$CYCLONEDDS_URI" configuration, or automatically choose one.
This option is not active by default, unless the "ROS_LOCALHOST_ONLY" environment variable is set to "1".'"#
        ));

    // Add option to enable DDS SHM if feature is enabled
    #[cfg(feature = "dds_shm")]
    {
        app = app.arg(Arg::from_usage(
r#"--dds-enable-shm \
'Configure CycloneDDS to use the Iceoryx shared memory PSMX plugin with default config. If not set, CycloneDDS will instead use any shared memory settings defined in "$CYCLONEDDS_URI" configuration.
This option is not active by default.'"#
            ));
    }

    app = app
        .arg(Arg::from_usage(
r#"-a, --allow=[String]...   'A regular expression matching the set of 'partition/topic-name' that must be routed via zenoh. By default, all partitions and topics are allowed.
If both '--allow' and '--deny' are set a partition and/or topic will be allowed if it matches only the 'allow' expression.
Repeat this option to configure several topic expressions. These expressions are concatenated with '|'.
Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"#
        ))
        .arg(Arg::from_usage(
r#"--deny=[String]...   'A regular expression matching the set of 'partition/topic-name' that must not be routed via zenoh. By default, no partitions and no topics are denied.
If both '--allow' and '--deny' are set a partition and/or topic will be allowed if it matches only the 'allow' expression.
Repeat this option to configure several topic expressions. These expressions are concatenated with '|'.
Examples of expressions: '.*/TopicA', 'Partition-?/.*', 'cmd_vel|rosout'...'"#
        ))
        .arg(Arg::from_usage(
r#"--max-frequency=[String]...   'Specifies a maximum frequency of data routing over zenoh for a set of topics. The string must have the format "<regex>=<float>":
  - "regex" is a regular expression matching the set of 'partition/topic-name' (same syntax than --allow option)
    for which the data (per DDS instance) must be routed at no higher rate than the specified max frequency.
  - "float" is the maximum frequency in Hertz; if publication rate is higher, downsampling will occur when routing.
Repeat this option to configure several topics expressions with a max frequency.'"#
        ))
        .arg(Arg::from_usage(
r#"-r, --generalise-sub=[String]...   'A list of key expression to use for generalising subscriptions (usable multiple times).'"#
        ))
        .arg(Arg::from_usage(
r#"-w, --generalise-pub=[String]...   'A list of key expression to use for generalising publications (usable multiple times).'"#
        ))
        .arg(Arg::from_usage(
r#"-f, --fwd-discovery   'When set, rather than creating a local route when discovering a local DDS entity, this discovery info is forwarded to the remote plugins/bridges. Those will create the routes, including a replica of the discovered entity.'"#
            ).alias("forward-discovery")
        )
        .arg(Arg::from_usage(
r#"--queries-timeout=[float]... 'A float in seconds (default: 5.0 sec) that will be used as a timeout when the bridge
queries any other remote bridge for discovery information and for historical data for TRANSIENT_LOCAL DDS Readers it serves
(i.e. if the query to the remote bridge exceed the timeout, some historical samples might be not routed to the Readers, but the route will not be blocked forever)."#
        ))
        .arg(Arg::from_usage(
r#"--watchdog=[PERIOD]   'Experimental!! Run a watchdog thread that monitors the bridge's async executor and reports as error log any stalled status during the specified period (default: 1.0 second)'"#
        ).default_missing_value("1.0"));
    let args = app.get_matches();

    // load config file at first
    let mut config = match args.value_of("config") {
        Some(conf_file) => Config::from_file(conf_file).unwrap(),
        None => Config::default(),
    };
    // if "dds" plugin conf is not present, add it (empty to use default config)
    if config.plugin("dds").is_none() {
        config.insert_json5("plugins/dds", "{}").unwrap();
    }

    // apply zenoh related arguments over config
    // NOTE: only if args.occurrences_of()>0 to avoid overriding config with the default arg value
    if args.occurrences_of("id") > 0 {
        config
            .set_id(Some(
                ZenohId::from_str(args.value_of("id").unwrap()).unwrap(),
            ))
            .unwrap();
    }
    if args.occurrences_of("mode") > 0 {
        config
            .set_mode(Some(args.value_of("mode").unwrap().parse().unwrap()))
            .unwrap();
    }
    if let Some(endpoints) = args.values_of("connect") {
        config
            .connect
            .endpoints
            .set(endpoints.map(|p| p.parse().unwrap()).collect())
            .unwrap();
    }
    if let Some(endpoints) = args.values_of("listen") {
        config
            .listen
            .endpoints
            .set(endpoints.map(|p| p.parse().unwrap()).collect())
            .unwrap();
    }
    if args.is_present("no-multicast-scouting") {
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
    }
    if let Some(port) = args.value_of("rest-http-port") {
        config
            .insert_json5("plugins/rest/http_port", &format!(r#""{port}""#))
            .unwrap();
    }
    // Always add timestamps to publications (required for PublicationCache used in case of TRANSIENT_LOCAL topics)
    config
        .timestamping
        .set_enabled(Some(ModeDependentValue::Unique(true)))
        .unwrap();
    // Enable admin space
    config.adminspace.set_enabled(true).unwrap();
    // Enable loading plugins
    config.plugins_loading.set_enabled(true).unwrap();

    // apply DDS related arguments over config
    insert_json5!(config, args, "plugins/dds/scope", if "scope",);
    insert_json5!(config, args, "plugins/dds/domain", if "domain", .parse::<u64>().unwrap());
    insert_json5!(config, args, "plugins/dds/localhost_only", if "dds-localhost-only");
    #[cfg(feature = "dds_shm")]
    {
        insert_json5!(config, args, "plugins/dds/shm_enabled", if "dds-enable-shm");
    }
    insert_json5!(config, args, "plugins/dds/allow", for "allow", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/dds/deny", for "deny", .collect::<Vec::<_>>());
    insert_json5!(config, args, "plugins/dds/max_frequencies", for "max-frequency", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/dds/generalise_pubs", for "generalise-pub", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/dds/generalise_subs", for "generalise-sub", .collect::<Vec<_>>());
    insert_json5!(config, args, "plugins/dds/queries_timeout", if "queries-timeout", .parse::<f64>().unwrap());
    if args.is_present("fwd-discovery") {
        config
            .insert_json5("plugins/dds/forward_discovery", "true")
            .unwrap();
    }

    let watchdog_period = if args.is_present("watchdog") {
        args.value_of("watchdog").map(|s| s.parse::<f32>().unwrap())
    } else {
        None
    };

    (config, watchdog_period)
}

#[tokio::main]
async fn main() {
    zenoh::init_log_from_env_or("z=info");
    tracing::info!("zenoh-bridge-dds {}", DDSPlugin::PLUGIN_LONG_VERSION);

    let (config, watchdog_period) = parse_args();
    tracing::info!("Zenoh {config:?}");

    if let Some(period) = watchdog_period {
        run_watchdog(period);
    }

    let mut plugins_mgr = PluginsManager::static_plugins_only();

    // declare REST plugin if specified in conf
    if config.plugin("rest").is_some() {
        plugins_mgr.declare_static_plugin::<zenoh_plugin_rest::RestPlugin, &str>("rest", true);
    }

    // declare DDS plugin
    plugins_mgr.declare_static_plugin::<zenoh_plugin_dds::DDSPlugin, &str>("dds", true);

    // create a zenoh Runtime.
    let mut runtime = match RuntimeBuilder::new(config)
        .plugins_manager(plugins_mgr)
        .build()
        .await
    {
        Ok(runtime) => runtime,
        Err(e) => {
            println!("{e}. Exiting...");
            std::process::exit(-1);
        }
    };
    if let Err(e) = runtime.start().await {
        println!("Failed to start Zenoh runtime: {e}. Exiting...");
        std::process::exit(-1);
    }

    futures::future::pending::<()>().await;
}

fn run_watchdog(period: f32) {
    let sleep_time = Duration::from_secs_f32(period);
    // max delta accepted for watchdog thread sleep period
    let max_sleep_delta = Duration::from_millis(50);
    // 1st threshold of duration since last report => debug info if exceeded
    let report_threshold_1 = Duration::from_millis(10);
    // 2nd threshold of duration since last report => debug warn if exceeded
    let report_threshold_2 = Duration::from_millis(100);

    assert!(
        sleep_time > report_threshold_2,
        "Watchdog period must be greater than {} seconds",
        report_threshold_2.as_secs_f32()
    );

    // Start a Liveliness Monitor thread for tokio Runtime
    let (_task, monitor) = LivelinessMonitor::start(tokio::spawn);
    std::thread::spawn(move || {
        tracing::debug!(
            "Watchdog started with period {} sec",
            sleep_time.as_secs_f32()
        );
        loop {
            let before = SystemTime::now();
            std::thread::sleep(sleep_time);
            let elapsed = SystemTime::now().duration_since(before).unwrap();

            // Monitor watchdog thread itself
            if elapsed > sleep_time + max_sleep_delta {
                tracing::warn!(
                    "Watchdog thread slept more than configured: {} seconds",
                    elapsed.as_secs_f32()
                );
            }
            // check last LivelinessMonitor's report
            let report = monitor.latest_report();
            if report.elapsed() > report_threshold_1 {
                if report.elapsed() > sleep_time {
                    tracing::error!(
                        "Watchdog detecting tokio is stalled! No task scheduling since {} seconds",
                        report.elapsed().as_secs_f32()
                    );
                } else if report.elapsed() > report_threshold_2 {
                    tracing::warn!(
                        "Watchdog detecting tokio was not scheduling tasks during the last {} ms",
                        report.elapsed().as_micros()
                    );
                } else {
                    tracing::info!(
                        "Watchdog detecting tokio was not scheduling tasks during the last {} ms",
                        report.elapsed().as_micros()
                    );
                }
            }
        }
    });
}
