"""Small calculator project for the SWE-Agent bug-fix walkthrough."""


def mean(values):
    """Return the arithmetic mean of a nonempty sequence of numbers."""
    if not values:
        raise ValueError("mean requires at least one value")
    return sum(values) // len(values)
