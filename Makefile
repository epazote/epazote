.PHONY: all get test clean build cover compile goxc bintray

GO ?= go
BIN_NAME=epazote
GO_XC = ${GOPATH}/bin/goxc -os="freebsd openbsd netbsd solaris dragonfly darwin linux"
GOXC_FILE = .goxc.json
GOXC_FILE_LOCAL = .goxc.local.json
VERSION=$(shell git describe --tags --always)

all: clean build

get:
	${GO} get

build: get
	${GO} get -u gopkg.in/yaml.v2;
	${GO} build -ldflags "-X main.version=${VERSION}" -o ${BIN_NAME} cmd/epazote/main.go;

clean:
	@rm -rf ${BIN_NAME} ${BIN_NAME}.debug *.out build debian

test: get
	${GO} test -race -v

cover:
	${GO} test -cover && \
	${GO} test -coverprofile=coverage.out  && \
	${GO} tool cover -html=coverage.out

compile: goxc

goxc:
	$(shell echo '{\n  "ConfigVersion": "0.9",' > $(GOXC_FILE))
	$(shell echo '  "AppName": "epazote",' >> $(GOXC_FILE))
	$(shell echo '  "ArtifactsDest": "build",' >> $(GOXC_FILE))
	$(shell echo '  "PackageVersion": "${VERSION}",' >> $(GOXC_FILE))
	$(shell echo '  "TaskSettings": {' >> $(GOXC_FILE))
	$(shell echo '    "bintray": {' >> $(GOXC_FILE))
	$(shell echo '      "downloadspage": "bintray.md",' >> $(GOXC_FILE))
	$(shell echo '      "package": "epazote",' >> $(GOXC_FILE))
	$(shell echo '      "repository": "epazote",' >> $(GOXC_FILE))
	$(shell echo '      "subject": "nbari"' >> $(GOXC_FILE))
	$(shell echo '    }\n  },' >> $(GOXC_FILE))
	$(shell echo '  "BuildSettings": {' >> $(GOXC_FILE))
	$(shell echo '    "LdFlags": "-X main.version=${VERSION}"' >> $(GOXC_FILE))
	$(shell echo '  }\n}' >> $(GOXC_FILE))
	$(shell echo '{\n "ConfigVersion": "0.9",' > $(GOXC_FILE_LOCAL))
	$(shell echo ' "TaskSettings": {' >> $(GOXC_FILE_LOCAL))
	$(shell echo '  "bintray": {\n   "apikey": "$(BINTRAY_APIKEY)"' >> $(GOXC_FILE_LOCAL))
	$(shell echo '  }\n } \n}' >> $(GOXC_FILE_LOCAL))
	${GO_XC}

bintray:
	${GO_XC} bintray
