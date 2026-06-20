```markdown
# remitlend-contracts Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches you the core development patterns and conventions used in the `remitlend-contracts` Rust codebase. You'll learn about file naming, import/export styles, commit message conventions, and how to write and run tests. While no explicit automation workflows were detected, this guide includes suggested commands for common development tasks.

## Coding Conventions

### File Naming
- Use **snake_case** for all file and module names.
  - Example:  
    ```plaintext
    loan_contract.rs
    user_profile.rs
    ```

### Import Style
- Use **relative imports** within the codebase.
  - Example:
    ```rust
    mod utils;
    use crate::models::loan;
    ```

### Export Style
- Use **named exports** for modules and functions.
  - Example:
    ```rust
    pub mod loan;
    pub fn calculate_interest() { ... }
    ```

### Commit Message Conventions
- Use **Conventional Commits** with clear prefixes.
- Common prefix: `refactor`
- Average commit message length: ~86 characters.
  - Example:
    ```
    refactor: split loan logic into separate modules for clarity and maintainability
    ```

## Workflows

_No explicit workflows detected in the repository. Below are suggested workflows for common development tasks._

### Refactor Code
**Trigger:** When improving code structure without changing behavior  
**Command:** `/refactor`

1. Identify code that can be improved for readability or maintainability.
2. Make changes, ensuring no external behavior is altered.
3. Use a commit message starting with `refactor:`.
4. Run tests to verify no regressions.

### Add New Module
**Trigger:** When introducing a new feature or domain concept  
**Command:** `/add-module`

1. Create a new `.rs` file using snake_case.
2. Define your module and public interfaces.
3. Add relative imports as needed.
4. Export the module in `lib.rs` or the parent module.
5. Write or update tests for the new module.

## Testing Patterns

- Test files are written in TypeScript with the pattern `*.test.ts`.
- The specific test framework is unknown, but standard Rust projects use `cargo test` for Rust code.
- Example test file name:
  ```
  loan_contract.test.ts
  ```
- Place test files alongside the code or in a dedicated test directory.

## Commands
| Command      | Purpose                                               |
|--------------|-------------------------------------------------------|
| /refactor    | Refactor code for clarity or maintainability          |
| /add-module  | Add a new module to the codebase                      |
```
