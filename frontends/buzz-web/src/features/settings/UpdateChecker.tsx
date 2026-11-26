import { openUrl } from "@tauri-apps/plugin-opener";
import { isTauri } from "@tauri-apps/api/core";
import { useUpdaterContext } from "./hooks/UpdaterProvider";
import { Button } from "@/shared/ui/button";
import {
  SettingsOptionGroup,
  SettingsOptionRow,
} from "./ui/SettingsOptionGroup";
import { SettingsSectionHeader } from "./ui/SettingsSectionHeader";
export function UpdateChecker() {
  const { status, checkForUpdate, installAndRelaunch } = useUpdaterContext();
  const automaticUpdatesAvailable = isTauri();

  return (
    <section className="min-w-0" data-testid="settings-updates">
      <SettingsSectionHeader
        title="Software Updates"
        description="Keep Buzz up to date with the latest features and fixes."
      />

      <SettingsOptionGroup title="Update status">
        {!automaticUpdatesAvailable ? (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p className="text-sm font-medium">Web version</p>
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Buzz web is updated by the hosted deployment. Reload this page
                to pick up the latest version.
              </p>
            </div>
            <Button size="sm" onClick={() => window.location.reload()}>
              Reload Buzz
            </Button>
          </SettingsOptionRow>
        ) : null}

        {automaticUpdatesAvailable && status.state === "idle" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Check if a new version is available.
              </p>
            </div>
            <Button size="sm" onClick={checkForUpdate}>
              Check for Updates
            </Button>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "checking" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Checking for updates...
              </p>
            </div>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "up-to-date" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                You're on the latest version.
              </p>
            </div>
            <Button variant="outline" size="sm" onClick={checkForUpdate}>
              Check Again
            </Button>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "unavailable" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Automatic updates aren't available on this build. Download the
                latest release manually.
              </p>
            </div>
            <Button variant="outline" size="sm" onClick={checkForUpdate}>
              Check Again
            </Button>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "manual-required" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p className="text-sm font-medium">
                Update available — v{status.version}
              </p>
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                In-app updates aren't supported on this Linux package. Download
                the new version from GitHub.{" "}
                <span>Switch to the AppImage build for automatic updates.</span>
              </p>
            </div>
            <Button size="sm" onClick={() => void openUrl(status.releaseUrl)}>
              Download Update
            </Button>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "available" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Preparing update...
              </p>
            </div>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "downloading" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Downloading update...
              </p>
            </div>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "installing" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Installing update...
              </p>
            </div>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "ready" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p
                className="text-sm font-normal text-muted-foreground/70"
                data-settings-subcopy
              >
                Update downloaded. Click to apply.
              </p>
            </div>
            <Button size="sm" onClick={installAndRelaunch}>
              Update Now
            </Button>
          </SettingsOptionRow>
        )}

        {automaticUpdatesAvailable && status.state === "error" && (
          <SettingsOptionRow>
            <div className="min-w-0">
              <p className="text-sm font-normal text-destructive">
                Update failed: {status.message}
              </p>
            </div>
            <Button variant="outline" size="sm" onClick={checkForUpdate}>
              Retry
            </Button>
          </SettingsOptionRow>
        )}
      </SettingsOptionGroup>
    </section>
  );
}
