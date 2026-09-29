# GAF Wi-Fi Vent protocol

Static analysis covers GAF Wi-Fi Vent app 2.1 and firmware image `GAFVent_030000.bin`. BLE state reads and ordinary controls were tested against one fan. Its identifier is redacted. No firmware update command was sent.

## Transport

- App bundle ID: `com.gaf.wifivent`.
- Setup joins the fan's `GAFVent_XXXX` access point.
- The app uses Wi-Fi TCP at `192.168.4.1` and Bluetooth LE. Both use the same command family.
- BLE service UUID: `00FF`; command characteristic UUID: `FF01`.
- Frames use `#<three-character-command><payload>\n`. The Wi-Fi OTA path can append binary data before the final line feed.
- The firmware image is 1,039,680 bytes; SHA-256: `badf3a57571fd66ca3df76eeaeb988549334ceff72b5c9f7e58082732089f260`. It contains fan control, BLE GATT, a TLS TCP server, access-point and DHCP setup, and OTA partition writes. The TCP port is unknown.
- Firmware startup selects `WIFI_MODE_AP`. The app has no home-network credential, router connection, or reconnect flow. Station-mode code in ESP-IDF does not provide those app features.
- Firmware reports ESP-IDF `v3.1-dev-1193-g64b56bef-dirty`.

## Commands

The app encodes setters with uppercase, zero-padded hexadecimal (`%04X`). Replies use lowercase hexadecimal (`%04x`) or decimal digits (`%1d`). Payload `0` in `amr` and `tmr` acknowledges the command. Both setters were acknowledged and read back. Accepted ranges are unknown.

| Request | Response | Meaning |
| --- | --- | --- |
| `#idg\n` | `#idr%s%s\n` | Read identity. First six decimal digits are firmware version (`030000` = 3.0.0). Redact the remaining identity. |
| `#dmg\n` | `#dmr\n` | Read mode: `a` automatic, `t` timer, `o` OTA. Second byte `f` or `n` is the controller fan flag. |
| `#sdg\n` | `#sdr%04x%04x\n` | Read temperature and humidity in tenths. |
| `#atg\n` | `#atr%04x%04x\n` | Read automatic temperature and humidity thresholds in tenths. |
| `#ttg\n` | `#ttr%04x%04x\n` | Read remaining and original timer minutes. The app converts them to seconds. |
| `#ams%04X%04X\n` | `#amr%1d\n` | Set automatic mode. Temperature tenths Fahrenheit, then humidity tenths percent. |
| `#tms%04X\n` | `#tmr%1d\n` | Set timer duration in minutes. The app rounds input seconds to minutes. |

The app also contains OTA and reboot commands: `ois`, `oms`, `ome`, and `rbs`; replies are `oir`, `osr`, `oer`, and `rbr`. Updraft does not expose these commands. The firmware contains an additional token, `#pptP`; its purpose is unknown.

## BLE state capture

On 2026-09-29, Updraft scanned for service `00FF`, connected to characteristic `FF01`, read five state fields, and disconnected. The device identifier and identity suffix are omitted.

| Request | Response | Payload | Decoded result |
| --- | --- | --- | --- |
| `idg` | `idr` | `030000…` | Firmware 3.0.0; identity redacted |
| `dmg` | `dmr` | `af` | Automatic mode; controller fan flag off |
| `sdg` | `sdr` | `03ca00aa` | 97.0°F, 17.0% humidity |
| `atg` | `atr` | `041a012c` | 105.0°F, 30.0% thresholds |
| `ttg` | `ttr` | `00000000` | 0 remaining, 0 original minutes |

A repeat probe returned the same five fields. One sensor reply changed to `03da00a0` (98.6°F, 16.0% humidity); the other four fields matched.

Temperature and humidity are four-character hexadecimal values in tenths. Timer fields contain remaining and original minutes. The fan flag is controller state.

## Control tests

1. Initial state: automatic mode, fan flag on, thresholds 105.0°F / 30.0%, timer 0/0.
2. `#ams041B012D\n` set thresholds to 105.1°F / 30.1%. Reply: `#amr0\n`. Readback: `#atr041b012d\n`. The fan flag then read off; sensors read 98.3°F / 16.9%.
3. `#tms0001\n` set a one-minute timer. Reply: `#tmr0\n`. Mode: timer/on. Timer readback: 1/1 minute.
4. `#tms0000\n` cleared the timer. Reply: `#tmr0\n`. Mode: timer/off. Timer readback: 0/0.
5. `#ams041A012C\n` restored automatic mode and thresholds. Reply: `#amr0\n`. Threshold readback: 105.0°F / 30.0%. Timer readback: 0/0. Final mode: automatic/off; sensors: 99.7°F / 15.8%.

The test restored the initial thresholds, mode, and timer. The fan flag changed from on to off.

## Remaining work

- Confirm the model and firmware revision from a product label or other source.
- Capture the Wi-Fi TLS exchange if implementing Wi-Fi support. Keep certificate and private-key contents out of the repository.
- Test other controls after identifying their command ranges. Measure airflow separately from controller state.

Home-LAN provisioning and Wi-Fi control are untested. BLE reads and threshold writes work over Bluetooth.
