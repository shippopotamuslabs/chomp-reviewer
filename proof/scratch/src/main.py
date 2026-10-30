"""Service entry point."""

import os

from src.parse import parse_port


def main():
    raw = os.environ.get("PORT", "")
    port = parse_port(raw)
    print(f"listening on {port}")


if __name__ == "__main__":
    main()
