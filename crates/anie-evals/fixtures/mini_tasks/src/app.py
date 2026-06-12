"""Task store and lifecycle for mini_tasks."""

from util import slugify


class TaskStore:
    def __init__(self):
        self.tasks = {}

    def get(self, slug):
        return self.tasks.get(slug)


def add_task(store, title):
    slug = slugify(title)
    store.tasks[slug] = {"title": title, "done": False}
    return slug


def complete_task(store, slug):
    task = store.tasks[slug]
    task["done"] = True
    return task
