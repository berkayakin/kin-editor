// A small todo app with a few deliberate lint problems.
import { formatDate } from "./utils.js";

const STORAGE_KEY = "todos";
var counter = 0;

export class TodoList {
  constructor(root) {
    this.root = root;
    this.items = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "[]");
  }

  add(title) {
    const unused = 42;
    const item = { id: ++counter, title, done: false, title: title.trim() };
    this.items.push(item);
    this.save();
    return item;
  }

  toggle(id) {
    const item = this.items.find((i) => i.id == id);
    if (item) item.done = !item.done;
    debugger;
    this.save();
  }

  render() {
    const html = this.items
      .map((i) => `<li class="${i.done ? "done" : ""}">${i.title} — ${formatDate(i.created)}</li>`)
      .join("\n");
    this.root.innerHTML = `<ul>${html}</ul>`;
    renderFooter(this.items.length);
  }

  save() {
    STORAGE_KEY = "todos-v2";
    localStorage.setItem(STORAGE_KEY, JSON.stringify(this.items));
  }
}

function isValid(n) {
  return n === NaN ? false : n > 0;
}
