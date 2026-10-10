import type { Repository } from "@/services/v1/openapi-types";

export type RepositoryEndpoint = {
  // The address a client points at, absolute so it can be copied as-is.
  url: string;
  // Where that address is configured in the ecosystem's own tooling.
  hint: string;
};

// getRepositoryEndpoint returns the call address for a repository by format,
// plus where the client configures it. The shape is the ecosystem's, not
// forklift's: cargo wants a sparse+ prefix, go wants no trailing slash, pypi
// wants the /simple/ index.
export function getRepositoryEndpoint(
  format: Repository["format"] | string,
  name: string,
  // Defaults to the origin the console is reached on, which is also the origin
  // the repositories are served from; passed explicitly by tests.
  origin?: string,
): RepositoryEndpoint {
  const base = origin ?? window.location.origin;

  switch (format) {
    case "maven":
      return {
        url: `${base}/maven/${name}/`,
        hint: "settings.xml <mirror><url>",
      };
    case "npm":
      return { url: `${base}/npm/${name}/`, hint: ".npmrc registry=" };
    case "cargo":
      return {
        url: `sparse+${base}/cargo/${name}/`,
        hint: ".cargo/config.toml [registries]",
      };
    case "go":
      return { url: `${base}/go/${name}`, hint: "GOPROXY=" };
    case "pypi":
      return {
        url: `${base}/pypi/${name}/simple/`,
        hint: "pip index-url / twine repository url",
      };
    case "oci":
      // OCI references are host-prefixed image names, not URLs: the docker
      // client always addresses /v2 at the host root, so the repository name
      // is the first segment of the image name.
      return {
        url: `${new URL(base).host}/${name}`,
        hint: "docker login / docker pull / helm push oci://",
      };
    default:
      return { url: `${base}/${format}/${name}/`, hint: "" };
  }
}
