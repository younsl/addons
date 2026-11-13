VERSION  := $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' Dockerfile | cut -d'"' -f2)
REGISTRY ?= ghcr.io/younsl
IMAGE    := $(REGISTRY)/jenkins-inbound-agent
PLATFORM ?= linux/amd64

.PHONY: build push clean

build:
	docker build --platform $(PLATFORM) -t $(IMAGE):$(VERSION) .

push:
	docker push $(IMAGE):$(VERSION)

clean:
	docker rmi $(IMAGE):$(VERSION)
