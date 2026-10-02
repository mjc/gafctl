# Original Master Flow controller: protocol findings

The original **GAF Master Flow Wi-Fi Attic Vent** models are **ERV5SMT**
(roof mount) and **EGV5SMT** (gable mount). Their original controller uses the
GAF Wi-Fi Vent app. QuickConnect uses a separate controller
and API; see [fan models](hardware.md) for manufacturer sources.

App: GAF Wi-Fi Vent 2.1 (`com.gaf.wifivent`). Firmware: `GAFVent_030000.bin`. BLE reads and controls were tested against one device; its identifier is redacted. No OTA command was sent.

## Transport

- App bundle ID: `com.gaf.wifivent`.
- Setup joins the fan's `GAFVent_XXXX` access point.
- The app uses Wi-Fi TCP at `192.168.4.1` and Bluetooth LE. Both use the same command family.
- BLE service UUID: `00FF`; command characteristic UUID: `FF01`.
- Frames use `#<three-character-command><payload>\n`. The Wi-Fi OTA path can append binary data before the final line feed.
- The firmware image is 1,039,680 bytes; SHA-256: `badf3a57571fd66ca3df76eeaeb988549334ceff72b5c9f7e58082732089f260`. It contains fan control, BLE GATT, a TLS TCP server, access-point and DHCP setup, and OTA partition writes. The TCP port is unknown.
- Firmware startup selects `WIFI_MODE_AP`. No home-network credentials, router connection, or reconnect flow were found.
- Home-LAN provisioning and Wi-Fi control are untested.
- Firmware reports ESP-IDF `v3.1-dev-1193-g64b56bef-dirty`.

## Commands

- Setter encoding: uppercase, zero-padded hexadecimal (`%04X`).
- Reply encoding: lowercase hexadecimal (`%04x`) or decimal digits (`%1d`).
- `amr` and `tmr` payload `0`: acknowledgement.
- Both setters acknowledged and read back. The controller's full range is
  untested; the adjustable settings and test results are below.

| Request | Response | Meaning |
| --- | --- | --- |
| `#idg\n` | `#idr%s%s\n` | Read identity. First six decimal digits are firmware version (`030000` = 3.0.0). Redact the remaining identity. |
| `#dmg\n` | `#dmr\n` | Read mode: `a` automatic, `t` timer, `o` OTA. Second byte `f` or `n` is the controller fan flag. |
| `#sdg\n` | `#sdr%04x%04x\n` | Read temperature and humidity in tenths. |
| `#atg\n` | `#atr%04x%04x\n` | Read automatic temperature and humidity thresholds in tenths. |
| `#ttg\n` | `#ttr%04x%04x\n` | Read remaining and original timer minutes. The app converts them to seconds. |
| `#ams%04X%04X\n` | `#amr%1d\n` | Set automatic mode. Temperature tenths Fahrenheit, then humidity tenths percent. |
| `#tms%04X\n` | `#tmr%1d\n` | Set timer duration in minutes. The app rounds input seconds to minutes. |

The app also contains OTA and reboot commands: `ois`, `oms`, `ome`, and `rbs`; replies are `oir`, `osr`, `oer`, and `rbr`. Gafctl does not expose these commands. The firmware contains an additional token, `#pptP`; its purpose is unknown.

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

On 2026-10-01, tests on the owner's controller confirmed 110°F, 40% humidity,
a two-minute timer, and timer clear, then restored automatic mode at 105°F / 30%. These checks
verified acknowledgement and readback, without measuring airflow. The
manufacturer Android app allows whole-unit settings of 90–120°F, 30–80%, and
1–360 timer minutes; timer clear was also tested. HTTP and HA expose
these bounded settings and preserve the untouched threshold during a change.
The range endpoints have not all been physically tested. The direct Bluetooth
CLI retains the four fixed presets in the earlier capture.

## Evidence limits

- Check additional controllers and firmware revisions. The captured device reports
  3.0.0; its identity reply does not identify the roof or gable model.
- Wi-Fi/TLS implementation and investigation remain paused. No home-LAN
  provisioning or Wi-Fi control tests are recorded.
- Adjustable ranges come from the app; the tested values are listed above.
  Motor operation and airflow require separate measurements.
