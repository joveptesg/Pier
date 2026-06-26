# Contributing to Pier

Thank you for your interest in contributing to Pier!

## License

Pier is licensed under [AGPL-3.0](LICENSE). By contributing, you agree that your contributions will be licensed under the same license.

## Contributor License Agreement (CLA)

Before we can accept your contribution, you must agree to our Contributor License Agreement. This is required to allow us to offer Pier under dual licensing (AGPL-3.0 for open source + commercial license for organizations).

- **Contributing your own work?** Agree to the [Individual CLA](CLA.md).
- **Contributing on behalf of an employer** that owns the copyright in your work (e.g. work made for hire)? Your employer must sign the [Corporate CLA](CLA-corporate.md).

## Trademark

The Pier code is open source, but the **"Pier" name and logo are trademarks** and are **not** covered by the AGPL. Please read the [Trademark Policy](TRADEMARK.md) before using the Pier name or logo for anything beyond running or referring to Pier.

## How to Contribute

1. **Fork** the repository
2. **Create a branch** for your feature or fix
3. **Write tests** for your changes
4. **Run tests** with `cargo test`
5. **Run clippy** with `cargo clippy -- -D warnings`
6. **Format** with `cargo fmt`
7. **Submit a pull request**

## Code Style

- Follow standard Rust conventions
- Run `cargo fmt` before committing
- Run `cargo clippy` and fix all warnings
- Write doc comments for public APIs
- Keep functions small and focused

## Reporting Issues

- Use GitHub Issues
- Include steps to reproduce
- Include system info (OS, Docker version, Pier version)

## Security Vulnerabilities

If you discover a security vulnerability, please **do not** open a public issue. Instead, email [info@devcom.app](mailto:info@devcom.app) with details.
