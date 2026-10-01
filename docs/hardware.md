# Fan models and compatibility

Updraft was built for the original **GAF Master Flow Wi-Fi Attic Vent**. GAF used
the Wi-Fi Attic Vent name for both the original product and the newer QuickConnect
product, so the app and controller generation matter when setting it up.

| Product | Models | Manufacturer app | Updraft connection |
| --- | --- | --- | --- |
| Master Flow Wi-Fi Attic Vent — Roof Mount | ERV5SMT | GAF Wi-Fi Vent | Direct Bluetooth |
| Master Flow Wi-Fi Attic Vent — Gable Mount | EGV5SMT | GAF Wi-Fi Vent | Direct Bluetooth |
| Master Flow Wi-Fi Attic Vent with QuickConnect — Roof Mount | ERV5QCT | GAF Master Flow QuickConnect | Experimental cloud API |
| Master Flow Wi-Fi Attic Vent with QuickConnect — Gable Mount | EGV5QCT | GAF Master Flow QuickConnect | Experimental cloud API |

The model numbers identify product families; retail SKUs may also include a finish.
This table identifies the hardware and connection methods. Updraft's device tests
cover one original controller, not every model, finish, or firmware revision.

## Original ERV5SMT and EGV5SMT

GAF's [2018 product instructions](https://images.thdstatic.com/catalog/pdfImages/20/2027af1f-49ef-4f20-83ce-3de23c8c00c5.pdf)
name ERV5SMT as the roof-mount model and EGV5SMT as the gable-mount model. They
describe a direct connection using the GAF Wi-Fi Vent app over Wi-Fi or Bluetooth.

The [GAF Wi-Fi Vent app's release notes](https://apps.apple.com/us/app/gaf-wi-fi-vent/id1388395737)
say firmware **3.0.0** adds Bluetooth Low Energy support. The controller tested
with Updraft reports that version. Updraft reads temperature, humidity, mode,
automatic thresholds, and timer state, and supports four fixed control presets.
See the [README](../README.md#add-it-to-home-assistant) for the available controls.

The original controller creates its own `GAFVent_XXXX` Wi-Fi access point. Updraft
uses Bluetooth, so the service computer can stay on your normal network. Updraft
has no implementation of this controller's direct Wi-Fi protocol.

The firmware's identity reply starts with a version and ends with a private
identifier. The current parser does not obtain a roof/gable model number from
that reply. A Bluetooth identifier alone does not distinguish ERV5SMT from EGV5SMT.

## QuickConnect

QuickConnect is GAF's newer Wi-Fi controller technology. It uses the
**GAF Master Flow QuickConnect** app, an account, and an Internet connection.
GAF's [QuickConnect product sheet](https://documents.gaf.com/data-sheets/master-flow-wi-fi-attic-vent-resmf314-%2811-22%29-_sell-sheet.pdf)
lists ERV5QCT and EGV5QCT. The
[current roof-mount product page](https://www.gaf.com/en-us/roofing-materials/residential-roofing-materials/attic-vents-other-ventilation/master-flow-wi-fi-attic-vent-roof-mount)
also describes QuickConnect; it is not a description of the original SMT controller.

Updraft's QuickConnect backend was implemented from a community integration's
source and synthetic test data. Live account and fan compatibility have not been
verified in this repository. It starts read-only; optional settings writes are
disabled by default. Instructions are in
[deployment](deployment.md#quickconnect-experimental), and the API research is in
[QuickConnect contract notes](quickconnect-contract.md).

## Other fans

Updraft does not currently have backends for QuietCool fans or for ordinary
Master Flow fans with a mechanical thermostat. A shared manufacturer or similar
product name does not establish controller compatibility.
