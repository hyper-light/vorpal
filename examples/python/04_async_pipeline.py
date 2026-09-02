"""Async bridge: index + fan-out searches without blocking the event loop.

    pip install vorpal-py
"""
import asyncio
import vorpal_py


async def main() -> None:
    generation = await vorpal_py.build(".", ".vorpal/index")
    print("generation:", generation)

    # One query...
    print(await vorpal_py.search(".vorpal/index", "http handler", 5))

    # ...or many, resolved concurrently on the Rust side.
    results = await vorpal_py.search_many(
        ".vorpal/index",
        ["database migration", "signal handling", "argument parsing"],
        k=3,
    )
    for query, rendered in zip(["database migration", "signal handling", "argument parsing"], results):
        print(f"== {query}\n{rendered}")

    # Graph + node lookups have async twins too.
    print(await vorpal_py.graph(".vorpal/index", "callers", "main"))


asyncio.run(main())
