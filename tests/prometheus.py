"""Read Rift's Prometheus text samples without rounding integer counters."""


def parse_metrics(text):
    samples = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, value = line.rsplit(None, 1)
        try:
            number = int(value)
        except ValueError:
            number = float(value)
        samples[name] = number
    return samples
