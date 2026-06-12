"""Small string helpers for mini_tasks."""


def slugify(title):
    """Turn a task title into a URL-friendly slug."""
    cleaned = "".join(c if c.isalnum() else "-" for c in title.lower())
    return "-".join(part for part in cleaned.split("-") if part)
