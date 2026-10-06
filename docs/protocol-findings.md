# Original Master Flow Bluetooth protocol

The original **GAF Master Flow Wi-Fi Attic Vent** models are **ERV5SMT**
(roof mount) and **EGV5SMT** (gable mount). Their original controller uses the
GAF Wi-Fi Vent app. QuickConnect uses a separate controller
and API; see [fan models](hardware.md) for manufacturer sources.

App: GAF Wi-Fi Vent 2.1 (`com.gaf.wifivent`). Firmware: `GAFVent_030000.bin`.
The captures below come from one controller reporting firmware 3.0.0. Device
identifiers are redacted.

## Transport

- App bundle ID: `com.gaf.wifivent`.
- Setup joins the fan's `GAFVent_XXXX` access point.
- The app uses Wi-Fi TCP at `192.168.4.1` and Bluetooth LE. Both use the same command family.
- BLE service UUID: `00FF`; command characteristic UUID: `FF01`.
- Enable `FF01` notifications before sending requests.
- Frames use `#<three-letter-command><payload>\n` with lowercase ASCII commands.
  Reassemble fragments through LF, then split complete frames for decoding.
  Gafctl rejects frames over 1024 bytes; the controller limit is unknown.
- The Wi-Fi OTA path can append binary data before the final line feed.
- The firmware image is 1,039,680 bytes; SHA-256: `badf3a57571fd66ca3df76eeaeb988549334ceff72b5c9f7e58082732089f260`. It contains fan control, BLE GATT, a TLS TCP server, access-point and DHCP setup, and OTA partition writes. The TCP port is unknown.
- Firmware startup selects `WIFI_MODE_AP`. No home-network credentials, router connection, or reconnect flow were found.
- Home-LAN provisioning and Wi-Fi control are untested.
- Firmware reports ESP-IDF `v3.1-dev-1193-g64b56bef-dirty`.

## Commands

- Setter encoding: uppercase, zero-padded hexadecimal (`%04X`).
- Reply encoding: lowercase hexadecimal (`%04x`) or decimal digits (`%1d`).
- `amr` and `tmr` payload `0`: acknowledgement.
- Both setters acknowledged and matched readback in the captures below.

| Request | Response | Meaning |
| --- | --- | --- |
| `#idg\n` | `#idr%s%s\n` | Read identity. First six decimal digits are firmware version (`030000` = 3.0.0). Redact the remaining identity. |
| `#dmg\n` | `#dmr<mode><fan>\n` | Read mode: `a` automatic, `t` timer, `o` OTA. Second byte `f` or `n` is the controller fan flag. |
| `#sdg\n` | `#sdr%04x%04x\n` | Read temperature and humidity in tenths. |
| `#atg\n` | `#atr%04x%04x\n` | Read automatic temperature and humidity thresholds in tenths. |
| `#ttg\n` | `#ttr%04x%04x\n` | Read remaining and original timer minutes. The app converts them to seconds. |
| `#ams%04X%04X\n` | `#amr%1d\n` | Set automatic mode. Temperature tenths Fahrenheit, then humidity tenths percent. |
| `#tms%04X\n` | `#tmr%1d\n` | Set timer duration in minutes. The app rounds input seconds to minutes. |

## Unused commands

The app contains OTA and reboot commands: `ois`, `oms`, `ome`, and `rbs`;
replies are `oir`, `osr`, `oer`, and `rbr`. These commands are unsupported in
Gafctl. The firmware token `#pptP` has an unknown purpose.

## Confirmation

Writes require an accepted acknowledgement and matching readback. Automatic
writes also require automatic mode. Timer writes require timer mode; clearing
also requires the fan flag off. Missing, malformed, mismatched or already-expired
readback leaves a write unconfirmed. Timer clear leaves timer mode active; write automatic thresholds to resume automatic operation.

The identity suffix is opaque. `controller_fan_on` reports the controller flag;
Gafctl keeps `estimated_running` null for this backend. Airflow is unmeasured.

## Timeouts and recovery

Scan defaults to six seconds. GATT setup, each request write and each response
wait default to three seconds. Platform adapter setup, scanning, connection and
cleanup allow at least 40 seconds for operating-system calls.

Transient connection failures retry after cleanup with exponential backoff and
jitter: 0.5–1, 1–2, 2–4 and 4–8 seconds. There are at most five attempts; retries
stop starting after 20 seconds. Authentication, protocol and cleanup errors stop
recovery. Requests already sent are never automatically replayed. Read current
state before retrying a timed-out control.

## BLE state capture

On 2026-09-29, Gafctl scanned for service `00FF`, connected to characteristic `FF01`, read five state fields, and disconnected. The device identifier and identity suffix are omitted.

| Request | Response | Payload | Decoded result |
| --- | --- | --- | --- |
| `idg` | `idr` | `030000…` | Firmware 3.0.0; identity redacted |
| `dmg` | `dmr` | `af` | Automatic mode; controller fan flag off |
| `sdg` | `sdr` | `03ca00aa` | 97.0°F, 17.0% humidity |
| `sdg` repeat | `sdr` | `03da00a0` | 98.6°F, 16.0% humidity |
| `atg` | `atr` | `041a012c` | 105.0°F, 30.0% thresholds |
| `ttg` | `ttr` | `00000000` | 0 remaining, 0 original minutes |

## Control tests

| Request | Reply | Readback | Controller state |
| --- | --- | --- | --- |
| Initial | — | thresholds 105.0°F / 30.0%; timer 0/0 | automatic/on |
| `#ams041B012D\n` | `#amr0\n` | `#atr041b012d\n` | 105.1°F / 30.1%; automatic/off; 98.3°F / 16.9% |
| `#tms0001\n` | `#tmr0\n` | timer 1/1 minute | timer/on |
| `#tms0000\n` | `#tmr0\n` | timer 0/0 | timer/off |
| `#ams041A012C\n` | `#amr0\n` | thresholds 105.0°F / 30.0%; timer 0/0 | automatic/off; 99.7°F / 15.8% |

## Adjustable control tests

On 2026-10-01, tests on the same controller confirmed 110°F, 40% humidity,
a two-minute timer, and timer clear, then restored automatic mode at 105°F / 30%. These checks
verified acknowledgement and readback, without measuring airflow. The
manufacturer Android app allows whole-unit settings of 90–120°F, 30–80%, and
1–360 timer minutes; timer clear was also tested. HTTP and HA expose
these bounded settings and preserve the untouched threshold during a change.
The range endpoints have not all been physically tested. The direct Bluetooth
CLI retains the four fixed presets in the earlier capture.

## Support limits

Hardware checks cover one firmware 3.0.0 controller. Its identity reply has no
roof/gable model field. The adjustable ranges come from the app; the tested
values are listed above. The diagnostic probe accepts raw `u16` values whose
full hardware range is untested.

Direct Wi-Fi/TLS control, manual mode, standalone on/off, firmware update, reboot
and reset are unsupported. The Wi-Fi listener port and authentication are
unknown. The tested Bluetooth session recorded no pairing exchange.

See the [HTTP API](http-api.md) for normalized state and service commands, and
the [QuickConnect API](quickconnect-contract.md) for the cloud controller.
