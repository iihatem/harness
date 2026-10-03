import re


def title(text):
    return text.strip().title()


def slugify(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
