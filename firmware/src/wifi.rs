//! WiFi control: one thread owns the radio, everything else talks to it.
//!
//! Two modes matter for the product:
//!
//! * **setup** - no credentials yet: start an access point on 192.168.4.1 and let
//!   a phone open the portal there. Nothing is burned into the firmware.
//! * **joined** - credentials are in NVS: connect as a station. The access point
//!   stays up as well when `wifi.ap_fallback` is set, so the portal is reachable
//!   even if the router hands out an address the phone cannot reach.

use esp32_agent_core::error::{McpError, Result};
use esp32_agent_core::hardware::{WifiAp, WifiStatus};
use esp32_agent_core::logbuf::LogBuf;
use esp32_agent_core::Shared;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::ipv4;
use esp_idf_svc::netif::{EspNetif, NetifConfiguration};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub enum WifiCmd {
    Connect { ssid: String, password: String },
    Scan(Sender<Result<Vec<WifiAp>>>),
    Reconfigure,
}

static STATUS: OnceLock<Arc<Mutex<WifiStatus>>> = OnceLock::new();
static CMD: OnceLock<Arc<Mutex<Option<Sender<WifiCmd>>>>> = OnceLock::new();

fn status_slot() -> Arc<Mutex<WifiStatus>> {
    STATUS
        .get_or_init(|| {
            Arc::new(Mutex::new(WifiStatus {
                state: "off".into(),
                ap_ip: "192.168.4.1".into(),
                ..Default::default()
            }))
        })
        .clone()
}

pub fn status() -> WifiStatus {
    match status_slot().lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

fn publish(update: impl FnOnce(&mut WifiStatus)) {
    let slot = status_slot();
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    update(&mut g);
}

/// Send a command to the radio thread. Returns immediately except for `Scan`,
/// which waits for the sweep to finish.
pub fn command(cmd: WifiCmd) -> Result<()> {
    let slot = CMD.get_or_init(|| Arc::new(Mutex::new(None))).clone();
    let guard = match slot.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let Some(tx) = guard.as_ref() else {
        return Err(McpError::hardware("the WiFi thread is not running yet"));
    };
    match cmd {
        WifiCmd::Scan(_) => Err(McpError::unsupported(
            "use `wifi_scan` (synchronous); the command channel does not carry results",
        )),
        other => tx.send(other).map_err(|_| McpError::hardware("WiFi thread gone")),
    }
}

/// Blocking scan, used by `Hardware::wifi_scan`.
pub fn scan() -> Result<Vec<WifiAp>> {
    let (tx, rx) = channel();
    let slot = CMD.get_or_init(|| Arc::new(Mutex::new(None))).clone();
    {
        let guard = match slot.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let Some(sender) = guard.as_ref() else {
            return Err(McpError::hardware("the WiFi thread is not running yet"));
        };
        sender.send(WifiCmd::Scan(tx)).map_err(|_| McpError::hardware("WiFi thread gone"))?;
    }
    rx.recv_timeout(Duration::from_secs(20)).map_err(|_| McpError::hardware("WiFi scan timed out"))?
}

/// Start the radio thread. It owns the driver for its whole life.
///
/// The modem, the system event loop and the NVS partition are all handed in: each
/// of them is a process-wide singleton whose second `take()` fails, and taking a
/// second partition was what used to kill this thread before it touched the radio.
pub fn start(
    app: Shared,
    log: Arc<LogBuf>,
    modem: Modem<'static>,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> anyhow::Result<()> {
    let (tx, rx) = channel::<WifiCmd>();
    *CMD.get_or_init(|| Arc::new(Mutex::new(None))).lock().unwrap() = Some(tx);

    // Named `radio`, not `wifi`: the IDF driver already owns two tasks called `wifi` and
    // `tiT`, and a third one under the same name makes `ps` unreadable. Internal RAM: this
    // thread reaches NVS through the driver.
    crate::threads::spawn(b"radio\0", 12 * 1024, crate::threads::INTERNAL, None, move || {
        if let Err(e) = run(app, log.clone(), rx, modem, sys_loop, nvs) {
            // Both sinks on purpose: the serial console is the only channel
            // that exists when the portal never came up, which is exactly the
            // failure this line is here to make visible.
            log::error!("radio thread stopped: {e}");
            log.error("wifi", format!("radio thread stopped: {e}"));
            publish(|s| s.state = "error".into());
        }
    })?;
    Ok(())
}

fn run(
    app: Shared,
    log: Arc<LogBuf>,
    rx: std::sync::mpsc::Receiver<WifiCmd>,
    modem: Modem<'static>,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> anyhow::Result<()> {
    let mut wifi = match EspWifi::new(modem, sys_loop, Some(nvs)) {
        Ok(w) => w,
        Err(e) => {
            log::error!("EspWifi::new failed: {e}");
            return Err(e.into());
        }
    };
    log::info!("radio driver created");

    let (
        ssid,
        password,
        ap_ssid,
        ap_password,
        ap_fallback,
        keep_ap,
        auto_reconnect,
        static_ip,
        static_netmask,
        static_gateway,
        static_dns,
    ) = {
        let guard = app.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        (
            guard.cfg.wifi.ssid.clone(),
            guard.cfg.wifi.password.clone(),
            guard.cfg.wifi.ap_ssid.clone(),
            guard.cfg.wifi.ap_password.clone(),
            guard.cfg.wifi.ap_fallback,
            guard.cfg.wifi.keep_ap,
            guard.cfg.wifi.auto_reconnect,
            guard.cfg.wifi.ip.clone(),
            guard.cfg.wifi.netmask.clone(),
            guard.cfg.wifi.gateway.clone(),
            guard.cfg.wifi.dns.clone(),
        )
    };
    let mut ssid = ssid;
    let mut ap_ssid = ap_ssid;

    // The softAP subnet comes from esp-idf-svc, whose default is
    // 192.168.71.0/24 (embedded-svc's `RouterConfiguration::default`), while the
    // captive DNS, the portal text and the README all say 192.168.4.1. A phone
    // following the portal would land on an address that is not there, so pin the
    // documented subnet before the interface is started.
    let ap_netif = EspNetif::new_with_conf(&ap_netif_config())?;
    // The old netif is unbound by the swap; dropping it here frees the driver's
    // original interface instead of leaking a second one.
    let _old_ap_netif = wifi.swap_netif_ap(ap_netif)?;

    let with_ap = ap_fallback || ssid.is_empty();
    let configuration = if ssid.is_empty() {
        Configuration::AccessPoint(ap_config(&ap_ssid, &ap_password))
    } else if with_ap {
        Configuration::Mixed(client_config(&ssid, &password), ap_config(&ap_ssid, &ap_password))
    } else {
        Configuration::Client(client_config(&ssid, &password))
    };
    log.info(
        "wifi",
        if ssid.is_empty() {
            format!("setup mode: access point `{ap_ssid}` at http://192.168.4.1/")
        } else {
            format!("joining `{ssid}`{}", if with_ap { " (AP kept up)" } else { "" })
        },
    );
    if let Err(e) = wifi.set_configuration(&configuration) {
        log::error!("set_configuration failed: {e}");
        return Err(e.into());
    }
    if let Err(e) = wifi.start() {
        log::error!("wifi.start failed: {e}");
        return Err(e.into());
    }
    // `start()` only brings the interfaces up. The station begins scanning and
    // associating when `connect()` is called, which is why "joined" mode used to
    // sit there with an idle radio.
    if !ssid.is_empty() {
        if let Err(e) = wifi.connect() {
            log::error!("wifi.connect failed: {e}");
        }
    }
    log::info!(
        "radio up: access point `{ap_ssid}` on http://192.168.4.1/, station {}",
        if ssid.is_empty() { "not configured".to_string() } else { format!("joining `{ssid}`") }
    );
    apply_static_ip(&static_ip, &static_netmask, &static_gateway, &static_dns);

    let mut last_state = String::new();
    let mut reconnect_ticks = 0u32;
    let mut overrides_due = 0u32;
    // The setup AP is closed once, the first time the station gets an address. Closing it on
    // every tick would fight the driver; closing it again after a reconnect would flap.
    let mut ap_closed = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(WifiCmd::Connect { ssid: new_ssid, password }) => {
                log.info("wifi", format!("reconfiguring for `{new_ssid}`"));
                let cfg = Configuration::Mixed(
                    client_config(&new_ssid, password.as_str()),
                    ap_config("esp32-agent-setup", ""),
                );
                wifi.set_configuration(&cfg)?;
                // A disconnect/connect cycle is what actually picks up new
                // credentials on ESP-IDF.
                let _ = wifi.stop();
                wifi.start()?;
                if let Err(e) = wifi.connect() {
                    log::error!("wifi.connect failed: {e}");
                }
                ssid = new_ssid;
                ap_ssid = "esp32-agent-setup".into();
                // A new join starts over: the AP may be needed again while it associates.
                ap_closed = false;
                apply_static_ip(&static_ip, &static_netmask, &static_gateway, &static_dns);
            }
            Ok(WifiCmd::Scan(reply)) => {
                let result = scan_now(&mut wifi).map_err(|e| McpError::hardware(format!("{e}")));
                let _ = reply.send(result);
                continue;
            }
            Ok(WifiCmd::Reconfigure) => {
                let _ = wifi.stop();
                wifi.start()?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }

        let sta_up = wifi.is_up().unwrap_or(false);
        // One `get_ip_info` for all four fields it carries: address, netmask, gateway and the
        // resolvers. Reading the config instead is what made `ifconfig` print an empty
        // netmask and gateway on a DHCP board — the values existed, nobody asked the interface.
        let sta_info = wifi.sta_netif().get_ip_info().ok();
        let sta_ip = sta_info.as_ref().map(|i| i.ip);
        let netmask = sta_info
            .as_ref()
            .filter(|i| !i.ip.is_unspecified())
            // The lease carries a prefix length (24), not an address; printing it as-is
            // reads like a netmask of "24", which is not a mask anyone can use.
            .map(|i| prefix_to_mask(i.subnet.mask.0))
            .unwrap_or_default();
        let gateway = sta_info
            .as_ref()
            .filter(|i| !i.ip.is_unspecified())
            .map(|i| i.subnet.gateway.to_string())
            .unwrap_or_default();
        let dns: Vec<String> = sta_info
            .as_ref()
            .map(|i| {
                [i.dns, i.secondary_dns]
                    .into_iter()
                    .flatten()
                    .filter(|ip| !ip.is_unspecified())
                    .map(|ip| ip.to_string())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        // Signal and channel come from the association, not from a default value: a board
        // reporting 0 dBm on channel 0 is reporting "never asked".
        let (rssi_dbm, channel) = sta_signal();
        let sta_mac = sta_mac();
        let sta_connected = sta_ip.map(|ip| !ip.is_unspecified()).unwrap_or(false);
        let ap_ip = wifi.ap_netif().get_ip_info().ok().map(|i| i.ip);
        // `ap_netif` keeps its address after the interface is stopped, so `Some` here only
        // means "it had one". Once the AP is closed the status must say so; a board that
        // reports `ap+sta` with 192.168.4.1 after closing the AP is the kind of lie this
        // whole session keeps tripping over.
        let ap_up = ap_ip.is_some() && !ap_closed;
        // The access point exists to get the board onto a network. Once the station has an
        // address it is closed — the portal shows that address, so there is nothing left for
        // the AP to do. `wifi.keep_ap` asks for both networks on purpose.
        if !keep_ap && !ap_closed && sta_connected && ap_up {
            let rc = unsafe { sys::esp_wifi_set_mode(sys::wifi_mode_t_WIFI_MODE_STA) };
            match rc {
                0 => {
                    ap_closed = true;
                    log.info(
                        "wifi",
                        format!(
                            "station up at {}; setup AP `{ap_ssid}` closed (wifi.keep_ap is off)",
                            sta_ip.map(|ip| ip.to_string()).unwrap_or_default()
                        ),
                    );
                }
                other => log.warn(
                    "wifi",
                    format!("could not close the setup AP (esp_wifi_set_mode returned {other}); it stays up"),
                ),
            }
        }
        let state = match (sta_connected, ap_up) {
            (true, true) => "ap+sta",
            (true, false) => "connected",
            (false, true) => "ap",
            (false, false) if sta_up => "connecting",
            (false, false) => "disconnected",
        }
        .to_string();
        let state_first = state.clone();
        publish(|s| {
            s.state = state.clone();
            s.ip = sta_ip.map(|ip| ip.to_string()).unwrap_or_default();
            s.ap_ip = match ap_ip {
                Some(ip) if !ap_closed => ip.to_string(),
                _ => String::new(),
            };
            s.ap_ssid = ap_ssid.clone();
            s.ssid = ssid.clone();
            s.hostname = "esp32-agent".into();
            s.rssi_dbm = rssi_dbm;
            s.channel = channel;
            s.mac = sta_mac.clone();
            s.gateway = gateway.clone();
            s.netmask = netmask.clone();
            s.dns = dns.clone();
            s.clients = if ap_up { ap_clients() } else { 0 };
        });
        // The console needs this line as much as the in-RAM log: a board whose
        // portal never appears is debugged over the serial port, and this is the
        // only place that says whether the radio actually moved.
        if state_first != last_state {
            log::info!(
                "wifi state: {state}, station ip {:?}, ap ip {:?}",
                sta_ip.map(|ip| ip.to_string()),
                ap_ip.map(|ip| ip.to_string())
            );
            last_state = state_first;
        }
        // Re-applied every 30 s from the live config: a gateway typed into the portal
        // should take effect without a reboot, and lwIP's DHCP client rewrites the DNS
        // servers from the lease on renewal, which would otherwise undo the setting.
        if sta_connected && overrides_due == 0 {
            let (gw, dns, dns2) = {
                let guard = app.lock().map_err(|_| anyhow::anyhow!("app lock poisoned"))?;
                (
                    guard.cfg.wifi.gateway.trim().to_string(),
                    guard.cfg.wifi.dns.trim().to_string(),
                    guard.cfg.wifi.dns2.trim().to_string(),
                )
            };
            apply_network_overrides(&gw, &dns, &dns2, wifi.sta_netif(), &log);
        }
        overrides_due = (overrides_due + 1) % 30;

        // `esp_wifi` only tries once per `connect()`; without this a router that
        // reboots leaves the board off the network until somebody reboots it too.
        if auto_reconnect && !ssid.is_empty() && !sta_connected {
            reconnect_ticks += 1;
            if reconnect_ticks >= 15 {
                reconnect_ticks = 0;
                log::info!("wifi state: reconnecting to `{ssid}`");
                if let Err(e) = wifi.connect() {
                    log::info!("wifi state: connect retry failed: {e}");
                }
            }
        } else {
            reconnect_ticks = 0;
        }
    }
    Ok(())
}

/// Point the station's traffic at a chosen gateway and resolver.
///
/// Both are empty by default, in which case the DHCP lease decides and this does nothing.
/// A gateway here is how a board is put behind a box that tunnels traffic: the lease still
/// supplies the address, and the default route is moved afterwards.
///
/// Re-applied periodically because lwIP's DHCP client writes the lease's DNS servers back
/// into the resolver list on renewal — a one-shot override would survive until the next
/// lease and then quietly stop being true.
fn apply_network_overrides(gateway: &str, dns: &str, dns2: &str, netif: &EspNetif, log: &Arc<LogBuf>) {
    // `RawHandle::handle` is the only way to the underlying esp_netif_t in this version.
    let raw = esp_idf_svc::handle::RawHandle::handle(netif);
    unsafe {
        let lwip = sys::esp_netif_get_netif_impl(raw) as *mut sys::netif;
        if lwip.is_null() {
            return;
        }
        if let Some(ip) = parse_v4(gateway) {
            let addr = sys::ip4_addr_t { addr: ip };
            sys::netif_set_gw(lwip, &addr);
            sys::netif_set_default(lwip);
        }
        for (index, text) in [(0u8, dns), (1u8, dns2)] {
            if let Some(ip) = parse_v4(text) {
                let mut info = sys::esp_netif_dns_info_t::default();
                info.ip.u_addr.ip4.addr = ip;
                let rc = sys::esp_netif_set_dns_info(
                    raw,
                    sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN + index as u32,
                    &mut info,
                );
                if rc != 0 {
                    log.warn("wifi", format!("dns {index} {text}: esp_netif_set_dns_info returned {rc}"));
                }
            }
        }
        if !gateway.is_empty() || !dns.is_empty() || !dns2.is_empty() {
            log.info(
                "wifi",
                format!(
                    "network overrides applied: gateway {} dns {} backup {}",
                    if gateway.is_empty() { "(lease)" } else { gateway },
                    if dns.is_empty() { "(lease)" } else { dns },
                    if dns2.is_empty() { "(lease)" } else { dns2 },
                ),
            );
        }
    }
}

/// `24` into `255.255.255.0`: the lease stores a prefix length and every consumer of a
/// netmask expects the dotted form.
fn prefix_to_mask(prefix: u8) -> String {
    let bits = u32::from(prefix.min(32));
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    format!("{}.{}.{}.{}", (mask >> 24) & 0xff, (mask >> 16) & 0xff, (mask >> 8) & 0xff, mask & 0xff)
}

/// Signal strength and channel of the association the station currently has.
///
/// `(0, 0)` means "not associated" rather than a measurement: 0 dBm is 1 mW at the antenna,
/// which no indoor link reaches, and channel 0 does not exist. The old code reported these
/// struct defaults as if they were readings, and a model reading them concluded the board
/// was lying — it was, about having measured anything.
fn sta_signal() -> (i32, u8) {
    let mut ap: sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
    if unsafe { sys::esp_wifi_sta_get_ap_info(&mut ap) } != 0 {
        return (0, 0);
    }
    (ap.rssi as i32, ap.primary)
}

fn sta_mac() -> String {
    let mut mac = [0u8; 6];
    if unsafe { sys::esp_wifi_get_mac(sys::wifi_interface_t_WIFI_IF_STA, mac.as_mut_ptr()) } != 0 {
        return String::new();
    }
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// How many phones are associated with the setup AP. Zero when it is closed, which is the
/// normal state after the station comes up.
fn ap_clients() -> u32 {
    let mut list: sys::wifi_sta_list_t = unsafe { std::mem::zeroed() };
    if unsafe { sys::esp_wifi_ap_get_sta_list(&mut list) } != 0 {
        return 0;
    }
    list.num.max(0) as u32
}

/// `a.b.c.d` to the network-order u32 lwIP wants. Anything else is "unset", not an error:
/// a typo in the portal should leave the lease in charge, not take the board off the LAN.
fn parse_v4(text: &str) -> Option<u32> {
    let mut parts = text.trim().split('.');
    let mut octets = [0u8; 4];
    for slot in octets.iter_mut() {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(u32::from_be_bytes(octets).to_be())
}

/// The setup access point's own netif: ESP-IDF's documented 192.168.4.0/24, with
/// the captive DNS advertised as the resolver so a joining phone asks us, not the
/// internet, where everything is.
///
/// The interface key cannot be `WIFI_AP_DEF`: the driver already registered one
/// under that name and `esp_netif_new` rejects a duplicate key.
fn ap_netif_config() -> NetifConfiguration {
    let mut conf = NetifConfiguration::wifi_default_router();
    conf.key = "WIFI_AP_SETUP".try_into().unwrap();
    conf.description = "apset".try_into().unwrap();
    conf.ip_configuration = Some(ipv4::Configuration::Router(ipv4::RouterConfiguration {
        subnet: ipv4::Subnet { gateway: ipv4::Ipv4Addr::new(192, 168, 4, 1), mask: ipv4::Mask(24) },
        dhcp_enabled: true,
        dns: Some(ipv4::Ipv4Addr::new(192, 168, 4, 1)),
        secondary_dns: Some(ipv4::Ipv4Addr::new(192, 168, 4, 1)),
    }));
    conf
}

fn scan_now(wifi: &mut EspWifi<'static>) -> anyhow::Result<Vec<WifiAp>> {
    let aps = wifi.scan()?;
    Ok(aps
        .into_iter()
        .take(24)
        .map(|ap| WifiAp {
            ssid: ap.ssid.to_string(),
            rssi: ap.signal_strength as i32,
            channel: ap.channel as u8,
            auth: format!("{:?}", ap.auth_method.unwrap_or(AuthMethod::None)),
        })
        .collect())
}

/// Put a configured address on the station, or leave DHCP alone.
///
/// `esp-idf-svc` 0.53 exposes `get_ip_info` but no setter, so this goes through
/// `esp-netif` directly. Order matters: the DHCP client has to stop first, otherwise the
/// lease it is about to receive overwrites the address a few milliseconds later. A bad
/// address is refused with a log line instead of half-applying — a board that quietly
/// keeps DHCP while the console shows a static address is worse than one that says no.
fn apply_static_ip(ip: &str, netmask: &str, gateway: &str, dns: &str) {
    let ip = ip.trim();
    if ip.is_empty() || ip == "0.0.0.0" {
        return;
    }
    let parse = |v: &str| -> Option<u32> {
        let mut parts = v.trim().split('.');
        let mut octets = [0u8; 4];
        for slot in octets.iter_mut() {
            *slot = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(u32::from_be_bytes(octets).to_be())
    };
    let mask = if netmask.trim().is_empty() { "255.255.255.0" } else { netmask };
    let (Some(ip_n), Some(mask_n)) = (parse(ip), parse(mask)) else {
        log::error!("wifi: static address `{ip}` or netmask `{mask}` is not dotted-quad; staying on DHCP");
        return;
    };
    let Some(gw_n) = parse(gateway) else {
        log::error!("wifi: static address `{ip}` has no usable gateway; staying on DHCP");
        return;
    };
    if (ip_n & mask_n) != (gw_n & mask_n) {
        log::error!("wifi: static address `{ip}` is outside the gateway's subnet; staying on DHCP");
        return;
    }
    unsafe {
        let handle = esp_idf_sys::esp_netif_get_handle_from_ifkey(
            b"WIFI_STA_DEF\0".as_ptr() as *const core::ffi::c_char,
        );
        if handle.is_null() {
            log::error!("wifi: no station netif to configure");
            return;
        }
        let _ = esp_idf_sys::esp_netif_dhcpc_stop(handle);
        let info = esp_idf_sys::esp_netif_ip_info_t {
            ip: esp_idf_sys::esp_ip4_addr_t { addr: ip_n },
            netmask: esp_idf_sys::esp_ip4_addr_t { addr: mask_n },
            gw: esp_idf_sys::esp_ip4_addr_t { addr: gw_n },
        };
        let rc = esp_idf_sys::esp_netif_set_ip_info(handle, &info);
        if rc != 0 {
            log::error!("wifi: esp_netif_set_ip_info failed ({rc}); staying on DHCP");
            return;
        }
        if let Some(dns_n) = parse(dns) {
            let mut dns_info: esp_idf_sys::esp_netif_dns_info_t = core::mem::zeroed();
            dns_info.ip.u_addr.ip4.addr = dns_n;
            dns_info.ip.type_ = esp_idf_sys::esp_ip_addr_type_t_ESP_IPADDR_TYPE_V4;
            let _ = esp_idf_sys::esp_netif_set_dns_info(
                handle,
                esp_idf_sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN,
                &mut dns_info,
            );
        }
        log::info!("wifi: static address `{ip}` applied, DHCP client stopped");
    }
}

fn client_config(ssid: &str, password: &str) -> ClientConfiguration {
    let mut cfg = ClientConfiguration::default();
    // ESP-IDF uses fixed-size SSID/password buffers (32 / 64 bytes); a longer
    // string is truncated rather than rejected, and an empty SSID means
    // "station not configured".
    cfg.ssid = ssid.try_into().unwrap_or_default();
    cfg.password = password.try_into().unwrap_or_default();
    cfg
}

fn ap_config(ssid: &str, password: &str) -> AccessPointConfiguration {
    let mut cfg = AccessPointConfiguration::default();
    cfg.ssid = ssid.try_into().unwrap_or_default();
    cfg.password = password.try_into().unwrap_or_default();
    cfg.auth_method = if password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };
    cfg.channel = 6;
    cfg
}
