# Contributing to Stronghold

Thank you for your interest in contributing to Stronghold! This document outlines the process for contributing to the project.

## Getting Started

1. **Fork** the repository on GitHub
2. **Clone** your fork locally
3. **Build** the project: `cargo build`
4. **Run tests**: `cargo test`

## Development Workflow

### Code Style

- Follow standard Rust formatting (`rustfmt`)
- Run `cargo fmt` before committing
- Run `cargo clippy` and fix all warnings
- Use meaningful commit messages (see below)

### Commit Messages

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <subject>

<body>

<footer>
```

Types:
- `feat`: A new feature
- `fix`: A bug fix
- `docs`: Documentation only changes
- `style`: Changes that do not affect the meaning of the code
- `refactor`: A code change that neither fixes a bug nor adds a feature
- `test`: Adding missing tests or correcting existing tests
- `chore`: Changes to the build process or auxiliary tools

### Pull Requests

1. Create a feature branch from `main`: `git checkout -b feat/my-feature`
2. Make your changes
3. Ensure `cargo fmt && cargo clippy && cargo test` pass
4. Write or update tests for your changes
5. Update documentation if needed
6. Submit a pull request

### Adding Images to the Catalog

1. Create a new directory under `images/<your-image-name>/`
2. Write an `image.toml` file following the [Image DSL spec](docs/IMAGE_DSL.md)
3. All images must `extends` from `stronghold/rocky-base`
4. Test the build locally: `stronghold image build images/<your-image-name>/image.toml`
5. Submit a PR

### Adding ADRs

Architecture Decision Records live in `docs/adr/`. To add one:

1. Copy `docs/adr/0000-template.md` to `docs/adr/NNNN-your-title.md`
2. Fill in the template
3. Submit a PR

## Security

If you discover a security vulnerability, please **do not** open a public issue. Instead, email security@stronghold.dev with a description of the vulnerability and steps to reproduce.

See [SECURITY.md](SECURITY.md) for the full security policy.

## Code of Conduct

Be respectful. Be constructive. Be inclusive.

## License

By contributing, you agree that your contributions will be licensed under the Apache-2.0 License.
