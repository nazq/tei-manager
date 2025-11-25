# Contributing to TEI Manager

[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](http://makeapullrequest.com)
[![Code of Conduct](https://img.shields.io/badge/code%20of%20conduct-contributor%20covenant-green.svg)](CODE_OF_CONDUCT.md)
[![First Timers](https://img.shields.io/badge/first--timers--only-friendly-blue.svg)](https://www.firsttimersonly.com/)

Thank you for considering contributing to TEI Manager! We welcome contributions from everyone.

---

## 📋 Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Setup](#development-setup)
- [Making Changes](#making-changes)
- [Testing](#testing)
- [Code Style](#code-style)
- [Commit Guidelines](#commit-guidelines)
- [Pull Request Process](#pull-request-process)
- [Release Process](#release-process)

---

## 📜 Code of Conduct

This project adheres to the Contributor Covenant Code of Conduct. By participating, you are expected to uphold this code. Please report unacceptable behavior to the project maintainers.

**Our Standards:**
- ✅ Be respectful and inclusive
- ✅ Welcome newcomers and help them learn
- ✅ Focus on what is best for the community
- ✅ Show empathy towards other community members
- ❌ No harassment, trolling, or discriminatory behavior

---

## 🚀 Getting Started

### Prerequisites

- **Rust 1.91+** with Edition 2024 support
- **Docker** (for containerized testing)
- **Git** for version control
- **jq** for JSON parsing in tests

### Finding Issues to Work On

- 🟢 **Good First Issue** - Perfect for newcomers
- 🟡 **Help Wanted** - We need your expertise
- 🔴 **Bug** - Something isn't working
- 🔵 **Enhancement** - New feature or improvement
- 📖 **Documentation** - Improvements or additions to docs

Browse [open issues](https://github.com/nazq/tei-manager/issues) or [create a new one](https://github.com/nazq/tei-manager/issues/new).

---

## 💻 Development Setup

### 1. Fork and Clone

```bash
# Fork on GitHub, then clone your fork
git clone https://github.com/YOUR_USERNAME/tei-manager.git
cd tei-manager

# Add upstream remote
git remote add upstream https://github.com/nazq/tei-manager.git
```

### 2. Install Dependencies

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update

# Verify Rust version
rustc --version  # Should be 1.91+

# Install development tools
rustup component add clippy rustfmt
```

### 3. Build the Project

```bash
# Debug build (faster compilation, slower runtime)
cargo build

# Release build (optimized)
cargo build --release

# Run locally
./target/debug/tei-manager --help
```

### 4. Run Tests

```bash
# Unit tests
cargo test

# Integration tests
cargo test --test '*'
```

---

## 🔧 Making Changes

### Branch Strategy

```bash
# Create a feature branch
git checkout -b feature/your-feature-name

# Or a bugfix branch
git checkout -b fix/issue-123-description
```

**Branch Naming:**
- `feature/` - New features
- `fix/` - Bug fixes
- `docs/` - Documentation updates
- `refactor/` - Code refactoring
- `test/` - Test improvements
- `chore/` - Maintenance tasks

### Development Workflow

1. **Make your changes** in focused, logical commits
2. **Write tests** for new functionality
3. **Update documentation** if you change APIs or behavior
4. **Run tests locally** before pushing
5. **Keep commits atomic** - one logical change per commit

---

## 🧪 Testing

### Running Tests

```bash
# All unit tests
cargo test

# Specific test
cargo test test_name

# With output
cargo test -- --nocapture
```

### Writing Tests

**Unit Tests** - Test individual functions/modules:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_validation() {
        let config = ManagerConfig {
            api_port: 500, // Below 1024
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
```

### Test Coverage Requirements

- ✅ All new code must have tests
- ✅ Bug fixes must include regression tests
- ✅ Code coverage should not decrease

---

## 🎨 Code Style

### Rust Style Guide

We follow the [Rust Style Guide](https://doc.rust-lang.org/nightly/style-guide/) with these specifics:

**Format Code:**
```bash
cargo fmt
```

**Lint Code:**
```bash
cargo clippy -- -D warnings
```

**Code Standards:**
- ✅ Use descriptive variable names
- ✅ Prefer `Result` over panics
- ✅ Add rustdoc comments for public APIs
- ✅ Use `tracing` for logging, not `println!`
- ✅ Avoid `unwrap()` - use proper error handling
- ✅ Use Edition 2024 features (let-else chains, etc.)

### Documentation

**Public APIs must have rustdoc:**

```rust
/// Creates a new TEI instance and starts it
///
/// # Arguments
///
/// * `config` - Instance configuration
///
/// # Returns
///
/// * `Ok(Arc<TeiInstance>)` - The created instance
/// * `Err(anyhow::Error)` - If creation or startup fails
///
/// # Example
///
/// ```
/// let instance = registry.add(config).await?;
/// ```
pub async fn add(&self, config: InstanceConfig) -> Result<Arc<TeiInstance>> {
    // ...
}
```

---

## 📝 Commit Guidelines

### Commit Message Format

```
<type>(<scope>): <subject>

<body>

<footer>
```

**Types:**
- `feat` - New feature
- `fix` - Bug fix
- `docs` - Documentation changes
- `style` - Code style changes (formatting, etc.)
- `refactor` - Code refactoring
- `test` - Adding or updating tests
- `chore` - Maintenance tasks

**Examples:**

```bash
feat(api): add DELETE /instances/:name endpoint

Implement instance deletion with proper cleanup of processes and state.

Closes #123
```

```bash
fix(health): prevent health check race condition

Health checks were starting before instances were fully initialized.
Added configurable initial delay to prevent false failures.

Fixes #456
```

```bash
docs(readme): add SPLADE example

Add example showing how to create and use SPLADE sparse models.
```

### Commit Best Practices

- ✅ Write clear, descriptive commit messages
- ✅ Keep commits focused - one logical change per commit
- ✅ Reference issues/PRs in commit messages
- ✅ Use present tense ("add feature" not "added feature")
- ✅ First line should be 50 chars or less
- ✅ Separate subject from body with blank line

---

## 🔄 Pull Request Process

### Before Submitting

1. ✅ **Sync with upstream:**
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. ✅ **Run all tests:**
   ```bash
   cargo test
   ```

3. ✅ **Run clippy:**
   ```bash
   cargo clippy -- -D warnings
   ```

4. ✅ **Format code:**
   ```bash
   cargo fmt
   ```

5. ✅ **Update documentation** if needed

### Creating a Pull Request

1. **Push to your fork:**
   ```bash
   git push origin feature/your-feature-name
   ```

2. **Open PR on GitHub** with a clear title and description

3. **Fill out the PR template:**
   - What does this PR do?
   - What issue does it fix?
   - How has it been tested?
   - Screenshots (if UI changes)
   - Checklist of completed items

### PR Review Process

1. **Automated checks** must pass:
   - ✅ Build succeeds
   - ✅ Tests pass
   - ✅ Linting passes
   - ✅ No security vulnerabilities

2. **Code review** by maintainers:
   - ✅ Code quality and style
   - ✅ Test coverage
   - ✅ Documentation completeness
   - ✅ No breaking changes (or properly documented)

3. **Address feedback:**
   - Make requested changes
   - Push new commits
   - Respond to review comments

4. **Merge:**
   - Squash and merge (for feature branches)
   - Rebase and merge (for hotfixes)

---

## 🚢 Release Process

See [release.sh](release.sh) for automated release process.

### Version Numbering

We use [Semantic Versioning](https://semver.org/):

- `MAJOR.MINOR.PATCH`
- **MAJOR** - Breaking changes
- **MINOR** - New features (backwards compatible)
- **PATCH** - Bug fixes (backwards compatible)

### Creating a Release

1. **Update version** in `Cargo.toml`
2. **Update CHANGELOG.md**
3. **Run release script:**
   ```bash
   ./release.sh 1.8.3
   ```
4. **Create GitHub release** with changelog

---

## 📚 Additional Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Tokio Tutorial](https://tokio.rs/tokio/tutorial)
- [Axum Documentation](https://docs.rs/axum/latest/axum/)

---

## ❓ Questions?

- 💬 Ask in [GitHub Discussions](https://github.com/nazq/tei-manager/discussions)
- 📧 Email maintainers
- 🐛 [Open an issue](https://github.com/nazq/tei-manager/issues/new)

---

## 🙏 Thank You!

Every contribution, no matter how small, makes a difference. Thank you for helping make TEI Manager better!

<div align="center">

[![Contributors](https://img.shields.io/github/contributors/nazq/tei-manager)](https://github.com/nazq/tei-manager/graphs/contributors)

</div>
