# Using Antigravity

Agent Terminal starts the Google-supplied `agy` executable already installed and
authenticated on your machine. It does not reimplement the Antigravity client, proxy
its service, or extract and reuse your Google credentials or OAuth tokens. In other
words, it uses the official CLI rather than trying to bypass it.

Google's current [Antigravity Terms](https://www.antigravity.google/terms) and
[FAQ](https://www.antigravity.google/docs/faq/) place restrictions on using
third-party software, tools or services to access Antigravity and say that violations
can lead to suspension or termination. Google has confirmed that it
[restricted accounts for third-party-tool use](https://discuss.ai.google.dev/t/update-on-antigravity-tos-ban/131424),
including tools that used unauthorised OAuth access instead of the supplied CLI.

This integration does **not** take that bypass route. However, Google has not published
a statement that distinguishes a GUI which drives `agy` from a prohibited
"third-party software, tool, or service." We cannot make that decision for Google or
guarantee its enforcement outcome. The integration may work today, but that is not an
approval, assurance of compliance, or guarantee of future availability. Review the
terms yourself and do not use this integration if they do not permit your intended use.

