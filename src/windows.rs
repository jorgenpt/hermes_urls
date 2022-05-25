// Copyright (c) Jørgen Tjernø <jorgen@tjer.no>. All rights reserved.
use anyhow::{anyhow, bail, Context, Result};
use log::{debug, error, info, trace, warn};
use mail_slot::{MailslotClient, MailslotName};
use simplelog::*;
use std::{
    fs::{File, OpenOptions},
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use structopt::StructOpt;
use winreg::{enums::*, RegKey};

// How many bytes do we let the log size grow to before we rotate it? We only keep one current and one old log.
const MAX_LOG_SIZE: u64 = 64 * 1024;

// Flags needed to run delete_subkey_all as well as just set_value and enum_values on the same handle.
const ENUMERATE_AND_DELETE_FLAGS: u32 = winreg::enums::KEY_READ | winreg::enums::KEY_SET_VALUE;

const DISPLAY_NAME: &str = "Hermes URL Handler";
const DESCRIPTION: &str = "Open links to UE4 assets or custom editor actions";

fn get_protocol_registry_key(protocol: &str) -> String {
    format!(r"SOFTWARE\Classes\{}", protocol)
}

fn get_configuration_registry_key(protocol: &str) -> String {
    format!(r"Software\bitSpatter\Hermes\Protocols\{}", protocol)
}

/// Register associations with Windows to handle our protocol, and the command we'll invoke
fn register_command(
    protocol: &str,
    #[allow(clippy::ptr_arg)] commandline: &Vec<String>,
    extra_args: Option<&str>,
) -> io::Result<()> {
    use std::env::current_exe;

    let exe_path = current_exe()?;
    let exe_path = exe_path.to_str().unwrap_or_default().to_owned();
    let icon_path = format!("\"{}\",0", exe_path);
    let open_command = if let Some(extra_args) = extra_args {
        format!("\"{}\" {} open \"%1\"", exe_path, extra_args)
    } else {
        format!("\"{}\" open \"%1\"", exe_path)
    };

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // Configure our ProgID to point to the right command
    let protocol_path = get_protocol_registry_key(protocol);
    let (progid_class, _) = hkcu.create_subkey(&protocol_path)?;
    progid_class.set_value("", &format!("URL:{} Protocol", protocol))?;

    // Indicates that this class defines a protocol handler
    progid_class.set_value("URL Protocol", &"")?;

    let (progid_class_defaulticon, _) = progid_class.create_subkey("DefaultIcon")?;
    progid_class_defaulticon.set_value("", &icon_path)?;

    debug!(
        r"set HKEY_CURRENT_USER\{}\DefaultIcon to '{}'",
        protocol_path, icon_path
    );

    let (progid_class_shell_open_command, _) = progid_class.create_subkey(r"shell\open\command")?;
    progid_class_shell_open_command.set_value("", &open_command)?;

    debug!(
        r"set HKEY_CURRENT_USER\{}\shell\open\command to '{}'",
        protocol_path, open_command
    );

    info!("registering command for {}://", protocol);
    let config_path = get_configuration_registry_key(protocol);
    let (config, _) = hkcu.create_subkey(&config_path)?;
    config.set_value("command", commandline)?;

    debug!(
        r"set HKEY_CURRENT_USER\{}\command to {:?}",
        config_path, commandline
    );

    Ok(())
}

/// Remove all the registry keys that we've set up for a protocol
fn unregister_protocol(protocol: &str) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let protocol_path = get_protocol_registry_key(protocol);
    trace!("querying protocol registration at {}", protocol_path);
    if let Ok(protocol_registry_key) =
        hkcu.open_subkey_with_flags(&protocol_path, ENUMERATE_AND_DELETE_FLAGS)
    {
        info!("removing protocol registration for {}://", protocol);

        let result = protocol_registry_key.delete_subkey_all("");
        if let Err(error) = result {
            warn!("unable to delete {}: {}", protocol_path, error);
        }
    } else {
        trace!(
            "could not open {}, assuming it doesn't exist",
            protocol_path,
        );
    }

    let _ = hkcu.delete_subkey(&protocol_path);

    let configuration_path = get_configuration_registry_key(protocol);
    trace!("querying configuration at {}", configuration_path);
    if let Ok(configuration_registry_key) =
        hkcu.open_subkey_with_flags(&configuration_path, ENUMERATE_AND_DELETE_FLAGS)
    {
        info!("removing configuration for {}://", protocol);

        let result = configuration_registry_key.delete_subkey_all("");
        if let Err(error) = result {
            warn!("unable to delete {}: {}", configuration_path, error);
        }
    } else {
        trace!(
            "could not open {}, assuming it doesn't exist",
            configuration_path,
        );
    }

    let _ = hkcu.delete_subkey(&configuration_path);
}

/// Combine the path and query string from the given Url
fn get_path_and_extras(url: &url::Url) -> String {
    let mut path = url.path().to_owned();

    if let Some(query) = url.query() {
        path += "?";
        path += query;
    }

    path
}

/// Dispatch the given URL to the correct mailslot or launch the editor
fn open_url(url: &str) -> Result<()> {
    let url = url::Url::parse(url)?;
    let protocol = url.scheme();
    let hostname = url
        .host_str()
        .ok_or_else(|| anyhow!("could not parse hostname from {}", url))?;
    let path = get_path_and_extras(&url);
    let full_path = format!("/{}{}", hostname, path);
    trace!(
        "split url {} into protocol={}, full_path={} (hostname={} + path={})",
        url,
        protocol,
        full_path,
        hostname,
        path
    );

    // Allow any process to steal focus from us, so that we will transfer focus "nicely" to
    // Unreal.
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
    }

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let config = hkcu
        .open_subkey(get_configuration_registry_key(protocol))
        .with_context(|| format!("no hostnames registered when trying to handle url {}", url))?;
    let protocol_command: Vec<_> = config
        .get_value("command")
        .with_context(|| format!("command not registered when trying to handle url {}", url))?;

    let could_send = {
        let slot = MailslotName::local(&format!(r"bitSpatter\Hermes\{}", protocol));
        trace!("Attempting to send URL to mailslot {}", slot.to_string());
        match MailslotClient::new(&slot) {
            Ok(mut client) => {
                if let Err(error) = client.send_message(full_path.as_bytes()) {
                    warn!("Could not send mail slot message to {}: {} -- assuming application is shutting down, starting a new one", slot.to_string(), error);
                    false
                } else {
                    trace!("Delivered using Mailslot");
                    true
                }
            }
            Err(mail_slot::Error::Io(io_error)) if io_error.kind() == ErrorKind::NotFound => {
                trace!("Mailslot not found, assuming application is not running");
                false
            }
            Err(err) => {
                error!(
                    "Could not connect to Mailslot, assuming application is not running: {:?}",
                    err
                );
                false
            }
        }
    };

    if !could_send {
        let (exe_name, args) = {
            debug!(
                "registered handler for {}: {:?}",
                protocol, protocol_command
            );
            let mut protocol_command = protocol_command.into_iter();
            let exe_name = protocol_command
                .next()
                .ok_or_else(|| anyhow!("empty command specified for hostname {}", hostname))?;

            // TODO: Handle %%1 as an escape?
            let args: Vec<_> = protocol_command
                .map(|arg: String| arg.replace("%1", &full_path))
                .collect();
            (exe_name, args)
        };

        info!("executing {:?} with arguments {:?}", exe_name, args);
        Command::new(&exe_name)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .with_context(|| format!("Failed to execute {:?} {:?}", exe_name, args))?;
    }

    Ok(())
}

/// Validate the scheme according to RFC3986 (https://datatracker.ietf.org/doc/html/rfc3986)
fn parse_scheme(src: &str) -> Result<String, anyhow::Error> {
    let src = src.trim();
    let mut chars = src.chars();
    let first_char = chars
        .next()
        .ok_or_else(|| anyhow!("protocol needs to contain at least one character"))?;
    if !first_char.is_ascii_alphabetic() {
        bail!(
            "protocol '{}' needs to start with an alphabetic character",
            src
        );
    }

    for char in chars {
        if !char.is_ascii_alphanumeric() && char != '+' && char != '-' && char != '.' {
            bail!("protocol '{}' can only contain the letters a-z, the numbers 0-9, '+', '-', and '.'", src);
        }
    }

    Ok(src.to_lowercase())
}

// This is the definition of our command line options
#[derive(Debug, StructOpt)]
#[structopt(
    name = DISPLAY_NAME,
    about = DESCRIPTION
)]
struct CommandOptions {
    /// Use verbose logging
    #[structopt(short, long)]
    verbose: bool,
    /// Use debug logging, even more verbose than --verbose
    #[structopt(long)]
    debug: bool,

    /// Choose the mode of operation
    #[structopt(subcommand)]
    mode: ExecutionMode,
}

#[derive(Debug, StructOpt)]
enum ExecutionMode {
    /// Dispatch the given URL to Unreal Engine (or launch it, if needed)
    Open {
        /// URL to open
        url: String,
    },

    /// Register this EXE as a URL protocol handler
    Register {
        /// The protocol this exe will be registered for
        #[structopt(parse(try_from_str = parse_scheme))]
        protocol: String,
        /// Enable debug logging for this registration
        #[structopt(long)]
        register_with_debugging: bool,
        /// The command line that will handle the registration if needed, where %1 is the placeholder for the path
        commandline: Vec<String>,
    },

    /// Remove all registry entries for the URL protocol handler & hostname configuration
    Unregister {
        /// The protocol we will delete the registration for
        #[structopt(parse(try_from_str = parse_scheme))]
        protocol: String,
    },
}

fn get_exe_relative_path(filename: &str) -> io::Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.set_file_name(filename);
    Ok(path)
}

fn rotate_and_open_log(log_path: &Path) -> Result<File, io::Error> {
    if let Ok(log_info) = std::fs::metadata(&log_path) {
        if log_info.len() > MAX_LOG_SIZE
            && std::fs::rename(&log_path, log_path.with_extension("log.old")).is_err()
            && std::fs::remove_file(log_path).is_err()
        {
            return File::create(log_path);
        }
    }

    return OpenOptions::new().append(true).create(true).open(log_path);
}

fn init() -> Result<CommandOptions> {
    // First parse our command line options, so we can use it to configure the logging.
    let options = CommandOptions::from_args();
    let log_level = if options.debug {
        LevelFilter::Trace
    } else if options.verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let mut loggers: Vec<Box<dyn SharedLogger>> = Vec::new();

    // Always log to hermes.log
    let log_path = get_exe_relative_path("hermes.log")?;
    loggers.push(WriteLogger::new(
        log_level,
        Config::default(),
        rotate_and_open_log(&log_path)?,
    ));

    // We only use the terminal logger in the debug build, since we don't allocate a console window otherwise.
    if cfg!(debug_assertions) {
        loggers.push(TermLogger::new(
            log_level,
            Config::default(),
            TerminalMode::Mixed,
        ));
    };

    CombinedLogger::init(loggers)?;
    trace!("command line options: {:?}", options);

    Ok(options)
}

fn get_debug_args(register_with_debugging: bool) -> Option<&'static str> {
    if register_with_debugging {
        Some("--debug")
    } else {
        None
    }
}

pub fn main() -> Result<()> {
    let options = init()?;
    trace!(
        "running from directory {}",
        std::env::current_dir().unwrap_or_default().display()
    );

    match options.mode {
        ExecutionMode::Register {
            protocol,
            commandline,
            register_with_debugging,
        } => {
            register_command(
                &protocol,
                &commandline,
                get_debug_args(register_with_debugging),
            )
            .with_context(|| format!("Failed to register command for {}://", protocol))?;
        }
        ExecutionMode::Unregister { protocol } => {
            info!("unregistering handler for {}://", protocol);
            unregister_protocol(&protocol);
        }
        ExecutionMode::Open { url } => {
            open_url(&url).with_context(|| format!("Failed to open url {}", url))?;
        }
    }

    Ok(())
}
