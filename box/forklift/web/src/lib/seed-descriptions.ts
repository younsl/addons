import type { Language } from "@/stores/user-preferences";

// Localized descriptions for the seeded repositories. The server stores the
// English text (src/repo/seed.rs keeps it in sync); the console swaps in
// the viewer's language here so the description never shows both languages at
// once. Custom repositories always render their stored description as-is.
const seedDescriptions: Record<Language, Record<string, string>> = {
  en: {
    "maven-central": "Caching proxy of Maven Central. Serves Maven and Gradle dependencies and keeps a local copy of every downloaded artifact.",
    "npmjs": "Caching proxy of the public npm registry. Package metadata and tarballs are cached locally after the first download.",
    "crates-io": "Caching proxy of the crates.io sparse index. Rust crates are cached locally so repeated builds avoid the upstream.",
    "goproxy": "Caching proxy of proxy.golang.org. Go modules and checksums are cached locally after the first fetch.",
    "pypi": "Caching proxy of PyPI. Python packages are cached locally so repeated installs avoid the upstream.",
    "docker.io-proxy": "Caching proxy of Docker Hub. Container images pulled through this registry are cached layer by layer.",
    "ghcr.io-proxy": "Caching proxy of the GitHub Container Registry. Container images and Helm charts pulled through this registry are cached layer by layer.",
    "maven-hosted": "Hosted repository for internal Maven and Gradle artifacts. Publish with mvn deploy or the console upload.",
    "npm-hosted": "Hosted repository for internal npm packages. Publish with npm publish or the console upload.",
    "cargo-hosted": "Hosted repository for internal Rust crates. Publish with cargo publish or the console upload.",
    "go-hosted": "Hosted repository for internal Go modules. Publish through the console upload.",
    "pypi-hosted": "Hosted repository for internal Python packages. Publish with twine or the console upload.",
    "oci-hosted": "Hosted OCI registry for internal container images and Helm charts. Push with standard clients such as docker and helm and oras.",
    "maven-public": "Group repository combining maven-hosted and maven-central behind one URL. Internal artifacts are looked up before the public proxy.",
    "npm-public": "Group repository combining npm-hosted and npmjs behind one URL. Internal packages are looked up before the public proxy.",
    "cargo-public": "Group repository combining cargo-hosted and crates-io behind one URL. Internal crates are looked up before the public proxy.",
    "go-public": "Group repository combining go-hosted and goproxy behind one URL. Internal modules are looked up before the public proxy.",
    "pypi-public": "Group repository combining pypi-hosted and pypi behind one URL. Internal packages are looked up before the public proxy.",
    "oci-public": "Group registry combining oci-hosted with the docker.io and ghcr.io proxies behind one image prefix. Internal images are looked up before the public registries.",
  },
  ko: {
    "maven-central": "Maven Central의 캐싱 프록시입니다. Maven과 Gradle 의존성을 제공하고 내려받은 아티팩트를 로컬에 보관합니다.",
    "npmjs": "공개 npm 레지스트리의 캐싱 프록시입니다. 패키지 메타데이터와 tarball을 최초 다운로드 후 로컬에 캐싱합니다.",
    "crates-io": "crates.io sparse 인덱스의 캐싱 프록시입니다. Rust 크레이트를 로컬에 캐싱해 반복 빌드가 업스트림을 거치지 않게 합니다.",
    "goproxy": "proxy.golang.org의 캐싱 프록시입니다. Go 모듈과 체크섬을 최초 조회 후 로컬에 캐싱합니다.",
    "pypi": "PyPI의 캐싱 프록시입니다. Python 패키지를 로컬에 캐싱해 반복 설치가 업스트림을 거치지 않게 합니다.",
    "docker.io-proxy": "Docker Hub의 캐싱 프록시입니다. 이 레지스트리를 거쳐 pull한 컨테이너 이미지를 레이어 단위로 캐싱합니다.",
    "ghcr.io-proxy": "GitHub Container Registry의 캐싱 프록시입니다. 이 레지스트리를 거쳐 pull한 컨테이너 이미지와 Helm 차트를 레이어 단위로 캐싱합니다.",
    "maven-hosted": "내부 Maven과 Gradle 아티팩트를 위한 호스티드 저장소입니다. mvn deploy 또는 콘솔 업로드로 게시합니다.",
    "npm-hosted": "내부 npm 패키지를 위한 호스티드 저장소입니다. npm publish 또는 콘솔 업로드로 게시합니다.",
    "cargo-hosted": "내부 Rust 크레이트를 위한 호스티드 저장소입니다. cargo publish 또는 콘솔 업로드로 게시합니다.",
    "go-hosted": "내부 Go 모듈을 위한 호스티드 저장소입니다. 콘솔 업로드로 게시합니다.",
    "pypi-hosted": "내부 Python 패키지를 위한 호스티드 저장소입니다. twine 또는 콘솔 업로드로 게시합니다.",
    "oci-hosted": "내부 컨테이너 이미지와 Helm 차트를 위한 호스티드 OCI 레지스트리입니다. docker와 helm 및 oras 같은 표준 클라이언트로 push합니다.",
    "maven-public": "maven-hosted와 maven-central을 하나의 URL로 묶은 그룹 저장소입니다. 내부 아티팩트를 공개 프록시보다 먼저 조회합니다.",
    "npm-public": "npm-hosted와 npmjs를 하나의 URL로 묶은 그룹 저장소입니다. 내부 패키지를 공개 프록시보다 먼저 조회합니다.",
    "cargo-public": "cargo-hosted와 crates-io를 하나의 URL로 묶은 그룹 저장소입니다. 내부 크레이트를 공개 프록시보다 먼저 조회합니다.",
    "go-public": "go-hosted와 goproxy를 하나의 URL로 묶은 그룹 저장소입니다. 내부 모듈을 공개 프록시보다 먼저 조회합니다.",
    "pypi-public": "pypi-hosted와 pypi를 하나의 URL로 묶은 그룹 저장소입니다. 내부 패키지를 공개 프록시보다 먼저 조회합니다.",
    "oci-public": "oci-hosted와 docker.io 및 ghcr.io 프록시를 하나의 이미지 prefix로 묶은 그룹 레지스트리입니다. 내부 이미지를 공개 레지스트리보다 먼저 조회합니다.",
  },
};

// repositoryDescription returns the description to display for a repository:
// the localized seed text for seeded repositories and the stored free text
// otherwise.
export function repositoryDescription(
  repo: { name: string; seeded?: boolean; description?: string },
  language: Language,
): string {
  if (repo.seeded) {
    return seedDescriptions[language]?.[repo.name] ?? repo.description ?? "";
  }
  return repo.description ?? "";
}
