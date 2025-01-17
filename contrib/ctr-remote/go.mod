module github.com/dragonflyoss/image-service/contrib/ctr-remote

go 1.16

require (
	github.com/containerd/containerd v1.5.9
	github.com/dragonflyoss/image-service/contrib/nydus-snapshotter v0.0.0-20210812024946-ec518a7d1cb8
	github.com/opencontainers/image-spec v1.0.2
	github.com/urfave/cli v1.22.5
)

replace (
	github.com/containerd/containerd => github.com/containerd/containerd v1.5.9
	github.com/opencontainers/image-spec => github.com/opencontainers/image-spec v1.0.2
	github.com/opencontainers/runc => github.com/opencontainers/runc v1.0.3
)
