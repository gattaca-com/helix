# Security Policy

## Reporting a Vulnerability

If you believe you have found a security vulnerability, please report it to us responsibly at security@gattaca.com


**Please DO NOT report security vulnerabilities through public GitHub issues.**

In your report, please include the following information:
- Description of the vulnerability
- Steps to reproduce the issue
- Potential impact
- Any possible mitigations


## Bug Bounty Program

We maintain a bug bounty program to reward security researchers who help us identify and fix vulnerabilities.


### Scope

In scope in `main` branch:
- Main application code
- API endpoints
- Authentication systems


### Rewards

Bounties are awarded based on severity, impact and likelihood:

| Severity |     Maximum | Example                                                                          |
|----------|------------:|----------------------------------------------------------------------------------|
| Medium   |  $1,000 USD | A bug that causes the relay to go offline                                        |
| High     | $15,000 USD | A bug that causes proposers to miss several slots                                |
| Critical | $25,000 USD | A bug that causes an untrusted proposer to access an invalid unblinded payload   |
