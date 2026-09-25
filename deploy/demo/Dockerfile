# 演示镜像：在官方发布镜像上叠加首次启动播种逻辑与示例数据。
# 中国大陆可改用 CNB 镜像源：FROM docker.cnb.cool/dbxio.com/dbx:latest
FROM t8y2/dbx:latest

RUN apt-get update && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

COPY seed/connections.json /seed/connections.json
COPY seed/entrypoint.sh /seed/entrypoint.sh
RUN chmod +x /seed/entrypoint.sh

# 先经官方入口脚本把数据卷交给非 root 用户 dbx 并降权，再运行播种脚本；
# 基础镜像尚无 dbx-entrypoint（旧版）时直接运行播种脚本。
ENTRYPOINT ["/bin/sh", "-c", "if [ -x /usr/local/bin/dbx-entrypoint ]; then exec /usr/local/bin/dbx-entrypoint /seed/entrypoint.sh; fi; exec /seed/entrypoint.sh"]
