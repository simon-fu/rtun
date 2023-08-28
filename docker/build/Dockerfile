FROM ubuntu:23.10

COPY rtun /usr/bin/rtun
RUN chmod +x /usr/bin/rtun

COPY entrypoint.sh /usr/bin/entrypoint.sh
RUN chmod +x /usr/bin/entrypoint.sh

# ENV PATH="/usr/bin:${PATH}"

ENTRYPOINT [ "/usr/bin/entrypoint.sh" ]
