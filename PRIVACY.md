# Privacy policy

Unclean is a local Windows application. It does not require an account and does not
send telemetry, analytics, crash reports, advertising data, or update requests.

## Data read

The application reads:

- Unreal Engine installation metadata from the Windows registry and launcher manifest;
- engine build metadata and plugin descriptors under selected engine roots;
- presets, application state, history, and backups created by Unclean;
- process metadata needed to warn about Unreal tools using the selected engine.

## Data written

Unclean writes local application state, presets, operation history, and backups under the
current user's application data directory. Confirmed engine operations may change
`EnabledByDefault` in selected plugin descriptors.

Logs and machine-readable output identify files when recovery requires it. They do not
include full descriptor contents unless the user requests an explicit diagnostic export.

## Network behavior

The application does not make network requests. Distribution sites, package managers, operating
systems, and code-signing services may process download or reputation data outside the
application. Their policies govern that activity.

Any future feature that sends data must ship with an updated policy that identifies the data,
recipient, purpose, retention, and user control before the feature is enabled.

## Data control

Users can inspect and delete Unclean's local state, presets, history, and backups. Deleting a
backup removes the application's automated restore source for that operation. Removing
application state does not revert descriptor changes.
