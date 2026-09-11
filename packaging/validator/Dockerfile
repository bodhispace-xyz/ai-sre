# Build-time downloads are separate from the credential-free, network-disabled validation run.
FROM ghcr.io/opentofu/opentofu:1.12.6-minimal@sha256:2fedbb3e956b083582543dabb76eba17e9f961dfd055311ea054a6191cfae9dd AS tofu
FROM docker:29.8.0-cli@sha256:eccaacfeed644c7de222ff047483568cb988dde95476fbaaf10ea2d04921bb66 AS docker
FROM docker/compose-bin:v5.5.1@sha256:14162382692aae43977f79effa64a499d61de19d4645461881daaf88773f7e74 AS compose
FROM python:3.14-slim-trixie@sha256:cad9a2c871761c413caa6fdd6441c783451e740a48aaeba60ae62a8b53525ef6 AS tools
RUN apt-get update && apt-get install -y --no-install-recommends bash git make ca-certificates \
    && apt-get clean
COPY --from=tofu /usr/local/bin/tofu /usr/local/bin/tofu
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
COPY --from=compose /docker-compose /usr/local/lib/docker/cli-plugins/docker-compose
RUN pip install --no-cache-dir ansible-core==2.21.3 PyYAML==6.0.3 \
    && ansible-galaxy collection install -p /opt/ansible/collections \
       community.docker:5.3.0 community.general:13.4.0 \
    && docker compose version --short | grep -Fx 5.5.1

# The named context must be a clean Git archive, never a credential-bearing checkout.
# Only provider configuration and its reviewed lock enter the build, not repository tests.
FROM tools AS providers
COPY --from=homelab /infra/opentofu/lxc/providers.tf /provider-input/providers.tf
COPY --from=homelab /infra/opentofu/lxc/.terraform.lock.hcl /provider-input/.terraform.lock.hcl
WORKDIR /provider-input
RUN tofu providers mirror /opt/providers

FROM tools AS validator
COPY --from=providers /opt/providers /opt/providers
COPY tofurc /etc/ai-sre-validator/tofurc
COPY --chmod=0555 validate-u9 /usr/local/bin/validate-u9
RUN mkdir /opt/ai-sre-validator \
    && pip freeze > /opt/ai-sre-validator/python-packages.txt \
    && dpkg-query -W > /opt/ai-sre-validator/os-packages.txt \
    && ansible-galaxy collection list -p /opt/ansible/collections > /opt/ai-sre-validator/ansible-collections.txt
USER 65532:65532
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/validate-u9"]
