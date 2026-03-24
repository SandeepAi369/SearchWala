FROM paulgoio/searxng:production

USER root

# Fix ownership for HF user 1000 (recursive covers all subdirs)
RUN chown -R 1000:1000 /usr/local/searxng \
    && mkdir -p /etc/searxng /var/cache/searxng \
    && chown -R 1000:1000 /etc/searxng \
    && chown -R 1000:1000 /var/cache/searxng

# Override paulgoio run.sh with our custom version
# (paulgoio uses sed -i which fails as user 1000)
COPY --chown=1000:1000 run.sh /usr/local/searxng/run.sh
RUN chmod +x /usr/local/searxng/run.sh

USER 1000

ENV GRANIAN_PORT=7860
ENV GRANIAN_HOST=0.0.0.0

EXPOSE 7860

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["./run.sh"]
