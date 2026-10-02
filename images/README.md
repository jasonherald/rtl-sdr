# Captured images

Images received off the air and decoded with this app, kept here so
they survive outside the machine that captured them. Folder layout and
file names are exactly what the app writes under `~/sdr-recordings/`.

They are a record, not test fixtures: nothing in the build or the tests
reads them. The pictures' content belongs to the originating services
(NOAA charts are US Government works in the public domain; ARISS
SSTV slides and Meteor-M imagery belong to their operators). The app's
MIT license does not apply to them.

## ISS SSTV (ARISS, 437.550 MHz)

- `sstv/sstv-ISS-ZARYA-2026-10-02-115850/img0.png`: the first ISS
  SSTV catch, "Student Education 2026". The bottom third is lost to a
  signal fade.
- `sstv/sstv-ISS-ZARYA-2026-10-02-182949/img0.png`: ARISS Series 33,
  slide 11/12, Sputnik (NA1SS / RS0ISS). The first pass with Doppler
  tracking and the 18 kHz channel (#927).
- `sstv/sstv-ISS-ZARYA-2026-10-02-182949/img1.png`: ARISS Series 33,
  "4 ОКТЯБРЯ 1957 … СПУТНИК-1".

## Meteor-M LRPT (137 MHz)

One folder per pass; each `apidNN.png` is one instrument channel.
APIDs 64 and 65 are visible light (black on night passes), 67 is
infrared. Horizontal black bands are packets lost while the signal
faded.

- `lrpt/lrpt-METEOR-M2-4-2026-09-26-155509/`: the first live LRPT
  images, daytime.
- `lrpt/lrpt-METEOR-M2-3-2026-09-28-220820/`: night pass, a long
  infrared strip in APID 67.
- `lrpt/lrpt-METEOR-M2-4-2026-09-30-160846/`: daytime pass.
- `lrpt/lrpt-METEOR-M2-4-2026-10-01-042731/`: night pass, infrared in
  APID 67.
- `lrpt/lrpt-METEOR-M2-4-2026-10-01-154655/`: daytime pass.

## WEFAX (NOAA NMF Boston, 4235 kHz)

The app saves WEFAX charts as RGB PNGs; these copies are greyscale
PNGs with identical pixel values (lossless, about a third of the size).

- `wefax/wefax-2026-09-22-084705.png`: the first legible charts, a
  hurricane-force surface analysis, a 500 mb analysis and forecasts,
  stacked in one image (before per-chart splitting, #921).
- `wefax/wefax-2026-09-26-223328.png`: the "radiofacsimile charts
  follow" broadcast header, from the first night with per-chart
  splitting (#926).
- `wefax/wefax-2026-09-26-225211.png`: broadcast schedule page.
- `wefax/wefax-2026-09-27-000015.png`: satellite image.
- `wefax/wefax-2026-09-27-010317.png`: tropical chart.
- `wefax/wefax-2026-09-27-041340.png`: surface analysis.
- `wefax/wefax-2026-09-27-043335.png`: 500 mb analysis.
- `wefax/wefax-2026-09-27-060019.png`: satellite image.
