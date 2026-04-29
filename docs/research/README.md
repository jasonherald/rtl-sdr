# Radio reception research notes

Personal research notes covering each radio-reception domain we're (eventually) building support for. Written before implementation as a knowledge dump — protocol details, RF setup, decoder pipelines, prior-art tools, references. Some of this is already shipped in the app; some is for future epics.

| File | Domain | Implementation status (as of April 2026) |
|---|---|---|
| `01-noaa-apt-weather-satellites.md` | NOAA APT (137 MHz weather sats) | shipped (epic #468) |
| `02-meteor-m-lrpt.md` | Meteor-M LRPT (137 MHz Russian weather sats) | shipped (epic #469) |
| `03-radiosonde-tracking.md` | Weather balloon RS-41 / DFM-17 / etc. | not yet — future epic |
| `04-pocsag-flex-pagers.md` | POCSAG / FLEX (VHF/UHF paging) | not yet — future epic |
| `05-sstv-slow-scan-tv.md` | SSTV (HF + ISS amateur image transmissions) | partial — ISS SSTV is epic #472 |
| `06-digital-trunked-radio.md` | P25 / DMR / NXDN / TETRA trunked systems | not yet — far-future epic |
| `07-acars-aviation-datalink.md` | ACARS (130 MHz aircraft text messages) | in progress — epic #474 (this is the design driver) |
| `08-vdl-mode-2.md` | VDL Mode 2 (137 MHz successor to ACARS) | not yet — future epic, follow-on to #474 |
| `09-inmarsat-iridium-satellites.md` | Inmarsat / Iridium L-band SATCOM | not yet — speculative future epic |

These are personal notes, not authoritative specs. For canonical protocol references, follow the links each doc provides.
