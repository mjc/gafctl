# GAF Wi-Fi Vent protocol findings

This document records static analysis of the GAF Wi-Fi Vent app and firmware, plus BLE queries and ordinary control writes against one fan. The device identifier is redacted. Controller responses report configuration and status; they do not measure airflow.

## Transport findings

- GAF Wi-Fi Vent app: version 2.1, bundle ID `com.gaf.wifivent`.
- The bundled setup instructions direct the phone to join the fan's `GAFVent_XXXX` Wi-Fi access point before opening the app.
- The app uses Wi-Fi TCP at `192.168.4.1` and Bluetooth LE. Both transport paths use the same text command family.
- BLE service UUID: `00FF`; command characteristic UUID: `FF01`.
- Text framing: `#<three-character-command><payload>\n`. The Wi-Fi OTA path can append a binary payload before the final line feed.
- The bundled `GAFVent_030000.bin` (1,039,680 bytes; SHA-256 `badf3a57571fd66ca3df76eeaeb988549334ceff72b5c9f7e58082732089f260`) is a structurally valid ESP32 application image. It contains GAF fan-control code, BLE GATT, a TLS TCP server, AP/DHCP setup, and OTA partition-writing code. Its listening port has not yet been recovered, and no OTA operation was sent to the device.
- Instruction-level analysis confirms the GAF application explicitly selects `WIFI_MODE_AP` during Wi-Fi startup. The image reports ESP-IDF `v3.1-dev-1193-g64b56bef-dirty`; the linked SDK contains generic station-mode code, but no GAF application flow for home SSID/password provisioning or router connection was found. Station mode is supported by the ESP32 SDK, so adding it appears technically feasible with firmware changes; changing the AP mode value alone would not implement credential setup, station connection, or reconnect handling. This describes app version 2.1 and the analyzed image. Other firmware revisions may differ.
- The image appears to be a custom GAF application built on ESP-IDF and its bundled open-source components. The `dirty` SDK version suffix indicates local changes in the SDK checkout at build time; it does not establish that the GAF application is a modified public project or that its source is available.

## Command inventory

`%04X` is uppercase, zero-padded hexadecimal as constructed by the app. `%04x` and `%1d` are firmware reply formatting. The app's response decoder establishes the read field order, scales, and units below. The app parser accepts the exact `amr` and `tmr` payload `0` as successful acknowledgements; other values are rejected as invalid data. Both ordinary setters have now received successful acknowledgements and matching readbacks on-device; accepted ranges remain unverified.

| Request | Response | Interpretation |
| --- | --- | --- |
| `#idg\n` | `#idr%s%s\n` | Read identity. First six decimal characters encode firmware version (`030000` = 3.0.0); the remaining identity is sensitive and must be redacted. |
| `#dmg\n` | `#dmr\n` | Read mode. First response character `a` = automatic, `t` = timer, `o` = OTA; second character `f`/`n` is the controller's fan-off/on flag. |
| `#sdg\n` | `#sdr%04x%04x\n` | Read temperature then humidity; both hexadecimal fields are tenths (`97.0°F`, `17.0%` in the observed capture). |
| `#atg\n` | `#atr%04x%04x\n` | Read automatic temperature then humidity thresholds; both are tenths (`105.0°F`, `30.0%` in the observed capture). |
| `#ttg\n` | `#ttr%04x%04x\n` | Read remaining timer minutes then original timer minutes; app converts them to seconds internally. |
| `#ams%04X%04X\n` | `#amr%1d\n` | Start automatic mode with temperature tenths Fahrenheit first, humidity tenths percent second. App scales each input by 0.1 before formatting. One write using the existing 105.0°F / 30.0% settings was acknowledged and read back unchanged. |
| `#tms%04X\n` | `#tmr%1d\n` | Start timer mode. Payload is duration in minutes; the app converts input seconds to rounded minutes before formatting. One-minute and zero-minute writes both succeeded; timer readback matched 1/1 and 0/0 respectively. |

The app also contains firmware-update and reboot operations: `ois`, `oms`, `ome`, and `rbs`, with replies `oir`, `osr`, `oer`, and `rbr`. They are documented for completeness and must not be used for ordinary discovery. The firmware has an additional unexplained `#pptP` token; it is not mapped to an app operation.

## Exact-device BLE capture and control verification

On 2026-09-29, Updraft scanned for service `00FF`, found one GAF BLE peripheral, connected to characteristic `FF01`, subscribed to notifications, sent the five getter commands, matched their replies, and disconnected. The peripheral identifier and identity suffix are omitted. Automatic-threshold and timer settings were changed and read back as described below. No OTA, reboot, reset, or pairing-change operation was sent.

| Request | Reply mnemonic | Redacted reply payload | App-decoded result |
| --- | --- | --- | --- |
| `idg` | `idr` | `030000…` | Firmware version field `030000` (3.0.0); remaining device identity redacted. |
| `dmg` | `dmr` | `af` | Automatic mode; controller reports fan off. This is not proof of motor state. |
| `sdg` | `sdr` | `03ca00aa` | `97.0°F`, `17.0%` relative humidity. |
| `atg` | `atr` | `041a012c` | `105.0°F`, `30.0%` thresholds. |
| `ttg` | `ttr` | `00000000` | 0 remaining minutes, 0 original minutes. |

A repeat run of the built CLI later the same day received all five replies again. The CLI represents payload bytes as hex, so sensor payload `3033646130306130` is the ASCII text `03da00a0`, which the app decoder reads as `98.6°F` and `16.0%`; threshold, mode, timer, and firmware-version fields were unchanged. The changing sensor values are consistent with live readings, while the repeated getters show the response path is reproducible.

Temperature and humidity fields are four-character hexadecimal values in tenths; the app displays temperature in Fahrenheit. Timer fields contain remaining and original minutes. The controller-reported fan flag and sensor values do not measure physical airflow.

The ordinary controls were exercised and then restored:

1. Before testing, the controller reported automatic/on, thresholds of 105.0°F / 30.0%, and a zero timer.
2. `#ams041B012D\n` changed thresholds slightly to 105.1°F / 30.1%. The device replied `#amr0\n` (the app parses this exact payload as ACK), and `atg` returned `#atr041b012d\n`. The controller then reported automatic/off; sensors were 98.3°F / 16.9%.
3. `#tms0001\n` started a one-minute timer. `#tmr0\n` was accepted as ACK; the controller reported timer/on and `ttg` returned one remaining and one original minute.
4. `#tms0000\n` cleared the timer. It returned `#tmr0\n`; the controller reported timer/off and `ttg` returned zero/zero.
5. `#ams041A012C\n` restored the original thresholds and automatic mode. It returned `#amr0\n`, `atg` read back 105.0°F / 30.0%, and `ttg` read zero/zero. The final controller report was automatic/off, with sensors at 99.7°F / 15.8%.

The test restored the original thresholds, zero timer, and automatic mode. The controller-reported fan flag changed from on to off. No firmware update command was sent; Updraft has no firmware-update operation.

## Tool scope and remaining work

The tool defaults to BLE and redacts device identity. It supports the listed state queries and ordinary threshold and timer writes, followed by state readback. It has no firmware-update operation.

Remaining work:

1. Confirm the exact fan/controller model and installed firmware revision independently of the redacted identity field.
2. Re-run identity/mode/sensor/threshold/timer reads and ordinary control tests as needed; retain redacted request/response bytes with timestamps.
3. Capture the Wi-Fi TLS exchange from the fan access point if implementing Wi-Fi support. Keep certificate and private-key contents out of the repository.
4. Extend the same acknowledgement and readback approach for other ordinary controls only after their ranges and effects are understood; do not treat a setter acknowledgement as proof of physical airflow.

Home-LAN Wi-Fi provisioning remains unconfirmed. BLE queries and a threshold write succeeded without joining the fan access point. Wi-Fi control is not implemented.
