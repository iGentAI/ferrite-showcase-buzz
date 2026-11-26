import manifestJson from "@features-manifest";
import { protectedFeatureDefinitions } from "@protected-features";
import { isTauri } from "@tauri-apps/api/core";
import { z } from "zod";
import type {
  FeatureDefinition,
  FeaturePlatform,
  FeaturesManifest,
} from "./types";

// Schema — runtime-validates the bundled preview-features.json at startup.
//
// On parse failure we fall back to an empty manifest and log a console warning.
// The app keeps working; gated UI stays hidden; nothing accidentally leaks.

const FeaturePlatformSchema = z.enum(["desktop", "mobile", "web"]);

const FeatureDefinitionSchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1),
  description: z.string(),
  defaultEnabled: z.boolean().optional(),
  platforms: z.array(FeaturePlatformSchema).optional(),
});

const FeaturesManifestSchema = z.object({
  version: z.number().int().nonnegative(),
  features: z.array(FeatureDefinitionSchema),
});

const EMPTY_MANIFEST: FeaturesManifest = { version: 1, features: [] };

function loadManifest(): FeaturesManifest {
  const result = FeaturesManifestSchema.safeParse({
    ...manifestJson,
    features: [...manifestJson.features, ...protectedFeatureDefinitions],
  });
  if (!result.success) {
    console.warn(
      "[FeatureFlags] preview-features.json failed schema validation; falling back to empty manifest.",
      result.error.issues,
    );
    return EMPTY_MANIFEST;
  }
  return result.data;
}

const manifest = loadManifest();

/** The validated manifest. Use `manifest.version` for cache/storage keys. */
export { manifest };

/** All features defined in the manifest */
export const allFeatures: FeatureDefinition[] = manifest.features;

/** Only features available on desktop */
export const desktopFeatures: FeatureDefinition[] = manifest.features.filter(
  (f) => !f.platforms || f.platforms.includes("desktop"),
);

/**
 * Which platform this build is RUNNING on.
 *
 * `isTauri()` is the same test the rest of the app uses (`App.tsx`,
 * `features/notifications/lib/desktop.ts`), so a feature's declared
 * availability and the capability checks around it cannot disagree about where
 * they are.
 */
function currentPlatform(): FeaturePlatform {
  return isTauri() ? "desktop" : "web";
}

/**
 * The preview features available on the platform this build is running on.
 *
 * A feature with no `platforms` list is available everywhere.
 *
 * This is what the Experiments settings card lists, and it must NOT be the
 * desktop list: filtering a web build to `desktop` leaves the card empty, and an
 * empty card is not a message — it is a settings page that silently offers
 * nothing, with every preview stuck at its default and no way to change it.
 */
export const availableFeatures: FeatureDefinition[] = manifest.features.filter(
  (f) => !f.platforms || f.platforms.includes(currentPlatform()),
);

/** Look up a feature by id */
export function getFeature(id: string): FeatureDefinition | undefined {
  return manifest.features.find((f) => f.id === id);
}
