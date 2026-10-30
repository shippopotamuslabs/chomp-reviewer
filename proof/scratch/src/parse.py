"""Order parsing helpers."""


def parse_port(raw):
    """Parse the PORT value leniently: blank means the default."""
    text = (raw or "").strip()
    if not text:
        return 8080
    return int(text)
