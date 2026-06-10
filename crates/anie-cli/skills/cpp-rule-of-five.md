---
name: cpp-rule-of-five
description: When implementing a C++ class that owns raw `new`/`delete` allocations or any non-trivially-managed resource, you must define all five special members (destructor, copy/move constructors, copy/move assignment). `= default` will leak (destructor) or double-free (copies).
license: MIT
---

# When this applies

You're writing or editing a C++ class that:
- Calls `new` / `delete` directly to manage memory.
- Owns a raw pointer that needs cleanup.
- Holds a file handle, socket, or any other resource the destructor must release.

OR — you're touching an existing class whose compile errors mention copy/move semantics, segfaults on copy, leaks under valgrind, or "use after free" warnings.

# Why `= default` is wrong here

The compiler's defaults assume **trivially copyable members**. When the class owns a raw pointer:

- **`~Class() = default;`** doesn't `delete` the pointer — leaks every allocation.
- **`Class(const Class&) = default;`** copies the pointer value (shallow copy) — both objects now own the same allocation, double-free on destruction.
- **`Class(Class&&) noexcept = default;`** moves the pointer but doesn't null the source — source's destructor still runs and frees memory the destination is still using.
- Same problems for the assignment operators.

Comments don't help. Writing `// nodes are deleted via clear()` next to `~Class() = default;` doesn't make the destructor call `clear()` — it's aspirational fiction. The code does what the code says.

# The five special members, done correctly

For a class `Class<T>` that owns `Node<T>* head_` (allocated via `new`), here's a working pattern:

## 1. Destructor

Walk the structure and `delete` every owned node.

```cpp
~Class() {
    while (head_) {
        Node<T>* next = head_->next;
        delete head_;
        head_ = next;
    }
}
```

If you have a `clear()` method that does this, the destructor can call `clear()`. But the destructor MUST do the work — `= default` will not.

## 2. Copy constructor

Deep-copy each node into a fresh allocation.

```cpp
Class(const Class& other) {
    for (const auto& value : other) {
        push_back(value);  // or whatever your insertion API is
    }
}
```

Ensure `push_back` (or equivalent) handles the empty-`this` case, since you start from a default-constructed shell.

## 3. Move constructor

Take ownership of the source's pointers, then null the source so its destructor doesn't double-free.

```cpp
Class(Class&& other) noexcept
    : head_(other.head_), tail_(other.tail_), size_(other.size_) {
    other.head_ = nullptr;
    other.tail_ = nullptr;
    other.size_ = 0;
}
```

`noexcept` is critical — the standard library makes assumptions about exception-free moves for performance.

## 4. Copy assignment — copy-and-swap idiom

```cpp
Class& operator=(const Class& other) {
    Class temp(other);  // copy via copy constructor (above)
    swap(temp);          // swap state with temporary
    return *this;
    // temp drops here, freeing the OLD state via destructor
}
```

Where `swap` is a member that swaps each owned field. You'll need to write `swap` too — it's three `std::swap` calls, no big deal.

## 5. Move assignment

```cpp
Class& operator=(Class&& other) noexcept {
    if (this != &other) {
        clear();  // free current state via destructor logic
        head_ = other.head_;
        tail_ = other.tail_;
        size_ = other.size_;
        other.head_ = nullptr;
        other.tail_ = nullptr;
        other.size_ = 0;
    }
    return *this;
}
```

# Verification

After implementing all five, compile **and run** with these test cases:

```cpp
{
    Class<int> a;
    a.push_back(1); a.push_back(2);
    Class<int> b(a);          // copy constructor
    Class<int> c(std::move(a)); // move constructor
    Class<int> d;
    d = b;                     // copy assignment
    Class<int> e;
    e = std::move(c);          // move assignment
    // All destructors run when scope ends — no double-free.
}
```

Run under valgrind or AddressSanitizer (`g++ -fsanitize=address`) — both should report no leaks and no errors.

# Anti-pattern checklist

Walk through your file before claiming done:

- [ ] Does the class call `new` anywhere? If yes, the destructor MUST manually `delete`.
- [ ] Are any of the five special members `= default` or omitted? If yes — fix.
- [ ] Does the destructor's behavior match what comments say it does?
- [ ] Did you actually run a copy / move test, or just trust the type system?

If any answer is "no" or "didn't test", the class isn't done.
