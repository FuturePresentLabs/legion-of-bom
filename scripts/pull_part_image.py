#!/usr/bin/env python3
"""Pull a part photo URL from a DIY supplier and cache it per-MPN in the parts library.

Deliberately an *internal script*, not part of the `lob` binary (5uj.6): the store
APIs and their keyword matching are fuzzy and can drift, so the fragile bit lives
here where it's cheap to fix, while `lob parts set-image` (the durable Dolt store)
stays small and generic.

Sources (all verified hotlinkable 2026-07-24):
  thonk   — Thonk WooCommerce Store API. Boutique Eurorack/DIY: Thonkiconn jacks,
            Alpha pots, knobs — the parts LCSC/EasyEDA don't carry.
  tayda   — Tayda Magento GraphQL. Broad DIY catalog: diodes, passives, pots,
            enclosures, hardware.
  easyeda — EasyEDA/LCSC product search. Generic ICs / catalog components.

Photos are pulled for our own build guides of kits we buy from these suppliers.

Usage:
  scripts/pull_part_image.py PJ398SM --source thonk
  scripts/pull_part_image.py 1N4148  --source tayda
  scripts/pull_part_image.py LM13700 --source auto --print-url
  scripts/pull_part_image.py PJ398SM --source thonk --query "thonkiconn" \
      --lob "cargo run -q -p legion-of-bom-cli --"

By default it calls `lob parts set-image <mpn> <url>`; --print-url just prints it.
"""

import argparse
import json
import shlex
import subprocess
import sys
import urllib.parse
import urllib.request

UA = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36"
)


def _get(url, headers=None):
    req = urllib.request.Request(url, headers={"User-Agent": UA, **(headers or {})})
    with urllib.request.urlopen(req, timeout=25) as resp:
        return json.loads(resp.read())


def thonk(query):
    """Thonk — WooCommerce Store API; first result's first image."""
    url = "https://www.thonk.co.uk/wp-json/wc/store/v1/products?per_page=5&search=" + \
        urllib.parse.quote(query)
    for product in _get(url):
        images = product.get("images") or []
        if images and images[0].get("src"):
            return images[0]["src"]
    return None


def tayda(query):
    """Tayda — Magento GraphQL; first product with a real (non-placeholder) image."""
    gql = '{products(search:"%s",pageSize:5){items{name small_image{url}}}}' % \
        query.replace('"', "")
    url = "https://www.taydaelectronics.com/graphql?query=" + urllib.parse.quote(gql)
    items = (_get(url).get("data", {}).get("products", {}) or {}).get("items", [])
    for item in items:
        img = (item.get("small_image") or {}).get("url")
        if img and "placeholder" not in img.lower():
            return img
    return None


def easyeda(query):
    """EasyEDA/LCSC — product search; first result that carries a photo."""
    url = "https://easyeda.com/api/eda/product/list?page=1&pageSize=8&keyword=" + \
        urllib.parse.quote(query)
    products = (_get(url, {"Referer": "https://easyeda.com/"}).get("result", {}) or {}) \
        .get("productList", [])
    for product in products:
        image = product.get("image") or []
        if image:
            for size in ("224x224", "900x900", "96x96"):
                if image[0].get(size):
                    return image[0][size]
    return None


SOURCES = {"thonk": thonk, "tayda": tayda, "easyeda": easyeda}
AUTO_ORDER = ["easyeda", "thonk", "tayda"]  # generic first, then boutique DIY


def resolve(source, query):
    for name in (AUTO_ORDER if source == "auto" else [source]):
        try:
            url = SOURCES[name](query)
        except Exception as exc:  # network / API drift — try the next source
            print(f"  {name}: {exc}", file=sys.stderr)
            url = None
        if url:
            return name, url
    return None, None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("mpn", help="MPN to key the photo under in the parts library")
    ap.add_argument("--source", choices=[*SOURCES, "auto"], default="auto")
    ap.add_argument("--query", help="search term (defaults to the MPN)")
    ap.add_argument("--print-url", action="store_true",
                    help="print the resolved URL; don't call lob")
    ap.add_argument("--lob", default="lob",
                    help='lob invocation (e.g. "cargo run -q -p legion-of-bom-cli --")')
    args = ap.parse_args()

    src, url = resolve(args.source, args.query or args.mpn)
    if not url:
        print(f"no image found for {args.query or args.mpn!r} "
              f"(source={args.source})", file=sys.stderr)
        sys.exit(1)
    print(f"{src}: {url}")
    if args.print_url:
        return
    subprocess.run(shlex.split(args.lob) + ["parts", "set-image", args.mpn, url],
                   check=True)


if __name__ == "__main__":
    main()
