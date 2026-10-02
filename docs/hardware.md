# Fan models and compatibility

Gafctl supports the original **GAF Master Flow Wi-Fi Attic Vent** controller over
Bluetooth and **Master Flow QuickConnect** controllers through the cloud API.
GAF used the Wi-Fi Attic Vent name for both generations. Identify the installed
controller and app when choosing a backend.

| Product | Models | Manufacturer app | Gafctl connection |
| --- | --- | --- | --- |
| Master Flow Wi-Fi Attic Vent — Roof Mount | ERV5SMT | GAF Wi-Fi Vent | Direct Bluetooth |
| Master Flow Wi-Fi Attic Vent — Gable Mount | EGV5SMT | GAF Wi-Fi Vent | Direct Bluetooth |
| Master Flow Wi-Fi Attic Vent with QuickConnect — Roof Mount | ERV5QCT | GAF Master Flow QuickConnect | Experimental cloud API |
| Master Flow Wi-Fi Attic Vent with QuickConnect — Gable Mount | EGV5QCT | GAF Master Flow QuickConnect | Experimental cloud API |
| Master Flow EZ Cool plug-in with QuickConnect — Roof Mount | EZCQCR1 | GAF Master Flow QuickConnect | Experimental cloud API |
| Master Flow EZ Cool plug-in with QuickConnect — Gable Mount | EZCQCG1 | GAF Master Flow QuickConnect | Experimental cloud API |
| Master Flow QuickConnect retrofit module | ERV/EGV series with the module installed | GAF Master Flow QuickConnect | Experimental cloud API |

The table includes GAF-documented model families across product revisions.
Retail availability may differ, and SKUs may also include a finish. A standard
ERV/EGV or EZ Cool fan needs a QuickConnect controller to use the cloud backend.
Gafctl's hardware tests cover one original controller. Other models, finishes
and firmware revisions are untested.

## Original ERV5SMT and EGV5SMT

GAF's [2018 product instructions](https://images.thdstatic.com/catalog/pdfImages/20/2027af1f-49ef-4f20-83ce-3de23c8c00c5.pdf)
name ERV5SMT as the roof-mount model and EGV5SMT as the gable-mount model. They
describe a direct connection using the GAF Wi-Fi Vent app over Wi-Fi or Bluetooth.

The [GAF Wi-Fi Vent app's release notes](https://apps.apple.com/us/app/gaf-wi-fi-vent/id1388395737)
say firmware **3.0.0** adds Bluetooth Low Energy support. The controller tested
with Gafctl reports that version. Gafctl reads temperature, humidity, mode,
automatic thresholds, and timer state. It supports fixed presets and adjustable
HTTP/HA controls.
See the [README](../README.md#add-it-to-home-assistant) for the available controls.

The original controller creates its own `GAFVent_XXXX` Wi-Fi access point. Gafctl
uses Bluetooth, so the service computer can stay on your normal network. Gafctl
has no implementation of this controller's direct Wi-Fi protocol.

The firmware's identity reply starts with a version and ends with a private
identifier. The current parser does not obtain a roof/gable model number from
that reply. A Bluetooth identifier alone does not distinguish ERV5SMT from EGV5SMT.

## QuickConnect

QuickConnect is GAF's newer Wi-Fi controller technology. It uses the
**GAF Master Flow QuickConnect** app, an account, and an Internet connection.
Manufacturer references checked on **2026-10-02**:

- GAF's [Wi-Fi Attic Vent product sheet, RESMF314](https://www.gaf.com/en-us/document-library/documents/data-sheets/master-flow-wi-fi-attic-vent-resmf314-%2811-22%29-_sell-sheet.pdf),
  page 2, identifies **ERV5QCT** and **EGV5QCT** with built-in QuickConnect.
- GAF's [powered ventilation warranty, RESWT189](https://www.gaf.com/en-us/document-library/documents/warranties/master-flow-powered-ventilation-products-limited-warranty-trilingual-reswt189.pdf),
  page 1, lists **EZCQCR1** and **EZCQCG1**. The April 2024 edition of its
  [EZ Cool product sheet, RESMF319](https://www.gaf.com/en-us/document-library/documents/data-sheets/master-flow-ez-cool-plug-in-power-vent-resmf319_data-sheet.pdf)
  identifies them as the QuickConnect roof and gable options. The October 2025
  edition at that URL omits the QuickConnect options.
- GAF's [ventilation catalog, RESCB100](https://www.gaf.com/en-us/document-library/documents/brochures-%26-literature/brochure__ventilation_full_line_brochure__rescb100.pdf),
  page 19, lists the **QuickConnect Wi-Fi Module** as an accessory for ERV/EGV
  series fans. Its [module instructions, RESMF332](https://www.gaf.com/en-us/document-library/documents/installation-instructions-%26-guides/master-flow-quickconnect-control-module-instructions-trilingual-resmf332-%283-23%29.pdf)
  describe replacing the thermostat or humidistat/thermostat controller.

Gafctl selects QuickConnect devices from the account inventory without filtering
by fan model. Built-in and retrofit QuickConnect controllers use this backend;
each device must return the fields described in the
[QuickConnect contract](quickconnect-contract.md).

Gafctl's QuickConnect backend was implemented from a community integration's
source and synthetic test data. Live account and fan compatibility have not been
verified in this repository. It starts read-only; optional settings writes are
disabled by default. Instructions are in
[deployment](deployment.md#quickconnect-experimental), and the API research is in
[QuickConnect contract notes](quickconnect-contract.md).

## Other fans

QuietCool fans and Master Flow fans with only mechanical thermostats are unsupported.
Other models need separate controller and protocol checks.
