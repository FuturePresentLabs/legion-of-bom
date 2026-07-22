"""RC low-pass filter — Phase 0 demo circuit #1.

A single-pole passive RC low-pass. This is a known-good textbook fixture: the
whole pipeline (SKiDL -> netlist -> parse -> simulate -> verify -> BOM) is
validated against its analytic behaviour before any real board depends on it.

Topology::

    IN ──[ R1 ]──┬── OUT
                 │
               [ C1 ]
                 │
                GND

Analytic behaviour (what the verify stage checks the ngspice AC sweep against):

    -3 dB cutoff:  fc = 1 / (2 * pi * R * C)

With R1 = 1 kΩ and C1 = 159 nF:

    fc = 1 / (2 * pi * 1e3 * 159e-9) ≈ 1.001 kHz

The verify stage recomputes fc from the parsed R and C values rather than
trusting a hard-coded number here, so these values are the single source of
truth. Change them and the expected cutoff follows automatically.

Run standalone (needs KICAD9_SYMBOL_DIR pointing at KiCad's symbol libraries)::

    KICAD9_SYMBOL_DIR=/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols \
      .venv/bin/python examples/rc_lowpass.py --output out/rc_lowpass/rc_lowpass.net

`lob run` sets that environment up for you; see docs/TOOLING.md.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

# Component values — the single source of truth for the expected cutoff.
R_VALUE = "1k"
C_VALUE = "159n"

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
C_FOOTPRINT = "Capacitor_SMD:C_0805_2012Metric"


def build():
    """Construct the RC low-pass in the default SKiDL circuit."""
    r1 = Part("Device", "R", value=R_VALUE, footprint=R_FOOTPRINT)
    c1 = Part("Device", "C", value=C_VALUE, footprint=C_FOOTPRINT)

    vin = Net("IN")
    vout = Net("OUT")
    gnd = Net("GND")

    vin += r1[1]
    vout += r1[2], c1[1]
    gnd += c1[2]

    return {"R1": r1, "C1": c1, "IN": vin, "OUT": vout, "GND": gnd}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit the RC low-pass KiCad netlist.")
    parser.add_argument(
        "--output",
        "-o",
        default="rc_lowpass.net",
        help="Path to write the generated KiCad netlist (default: ./rc_lowpass.net).",
    )
    args = parser.parse_args(argv)

    build()

    # First-pass validation (Section 4). ERC writes warnings/errors to stderr;
    # a clean RC filter produces none.
    ERC()

    generate_netlist(file_=args.output)
    print(f"wrote netlist: {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
