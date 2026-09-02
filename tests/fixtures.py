from __future__ import annotations

import io
from pathlib import Path

import pikepdf
import pymupdf
from pikepdf import Name
from PIL import Image


def write_transcript(
    path: Path, issue_date: str, terms: list[tuple[str, list[str]]]
) -> Path:
    """Write a positioned, synthetic transcript with no real-person content."""

    document = pymupdf.open()
    page = document.new_page(width=612, height=792)
    writer = pymupdf.TextWriter(page.rect)
    left = [
        "UNIVERSITY OF TEST",
        "OFFICIAL ACADEMIC TRANSCRIPT",
        "Student: REDACTED",
        f"Issued: {issue_date}",
        "Degree: Master of Science",
        "Degree Awarded: 2026-06-15",
    ]
    for row, text in enumerate(left):
        writer.append((40, 60 + row * 20), text, fontsize=10)

    row = 0
    for term, courses in terms:
        writer.append((350, 60 + row * 20), term, fontsize=10)
        row += 1
        writer.append(
            (350, 60 + row * 20),
            "Course ID Course Title Grade Units Points",
            fontsize=10,
        )
        row += 1
        for course in courses:
            writer.append((350, 60 + row * 20), course, fontsize=10)
            row += 1
        writer.append((350, 60 + row * 20), "Term GPA: 3.50", fontsize=10)
        row += 1
    writer.append((350, 60 + row * 20), "Transfer Credit: EXAMPLE COLLEGE", fontsize=10)
    writer.append(
        (350, 80 + row * 20), "HIST 100 World History B 3.00 9.00", fontsize=10
    )
    writer.write_text(page)
    document.save(path)
    document.close()
    return path


def write_single_column(path: Path) -> Path:
    document = pymupdf.open()
    page = document.new_page(width=612, height=792)
    page.insert_text((40, 60), "SINGLE COLUMN SAFE FIXTURE", fontsize=10)
    page.insert_text((40, 80), "No private content", fontsize=10)
    document.save(path)
    document.close()
    return path


def jpeg_bytes(mode: str, size: tuple[int, int]) -> bytes:
    """Encode one flat-colour JPEG in the requested PIL mode."""

    buffer = io.BytesIO()
    Image.new(mode, size).save(buffer, format="JPEG")
    return buffer.getvalue()


def write_image_pdf(path: Path, rgb_jpeg: bytes, cmyk_jpeg: bytes) -> Path:
    """Write two embedded JPEGs, the second behind a pre-multiplied soft mask.

    The second shape is the one that used to abort extraction: pikepdf
    declines it, and MuPDF folds the mask into an alpha channel it then
    refuses to re-encode as JPEG.
    """

    document = pymupdf.open()
    page = document.new_page(width=200, height=200)
    page.insert_image(pymupdf.Rect(10, 10, 110, 85), stream=rgb_jpeg)
    document.new_page(width=200, height=200)
    document.save(path)
    document.close()

    with pikepdf.open(path, allow_overwriting_input=True) as pdf:
        image = pikepdf.Stream(pdf, cmyk_jpeg)
        image.Type, image.Subtype = Name.XObject, Name.Image
        image.Width, image.Height, image.BitsPerComponent = 16, 12, 8
        image.ColorSpace, image.Filter = Name.DeviceCMYK, Name.DCTDecode
        image.DecodeParms = pikepdf.Dictionary(ColorTransform=1)
        mask = pikepdf.Stream(pdf, bytes(16 * 12))
        mask.Type, mask.Subtype = Name.XObject, Name.Image
        mask.Width, mask.Height, mask.BitsPerComponent = 16, 12, 8
        mask.ColorSpace = Name.DeviceGray
        mask.Matte = pikepdf.Array([0, 0, 0, 0])
        image.SMask = mask
        second = pdf.pages[1]
        second.Resources = pikepdf.Dictionary(XObject=pikepdf.Dictionary(Im0=image))
        second.Contents = pikepdf.Stream(pdf, b"q 100 0 0 75 10 100 cm /Im0 Do Q")
        pdf.save(path)
    return path


def write_empty_page_pdf(path: Path) -> Path:
    """Write a blank page, a text page, and a page that declares only a font.

    The third page draws nothing but keeps the second page's font resource.
    """

    document = pymupdf.open()
    document.new_page(width=200, height=200)
    page = document.new_page(width=200, height=200)
    page.insert_text((20, 40), "second page has text")
    document.save(path)
    document.close()

    with pikepdf.open(path, allow_overwriting_input=True) as pdf:
        text_page = pdf.pages[1]
        pdf.pages.append(
            pikepdf.Page(
                pdf.make_indirect(
                    pikepdf.Dictionary(
                        Type=Name.Page,
                        MediaBox=text_page.MediaBox,
                        Resources=text_page.Resources,
                        Contents=pikepdf.Stream(pdf, b""),
                    )
                )
            )
        )
        pdf.save(path)
    return path


def write_declined_image_pdf(path: Path) -> Path:
    """Write one page with two images pikepdf cannot extract.

    The first is a Flate stream of garbage bytes. The second is a spot-colour
    (`/Separation`) raster, which pikepdf refuses to transcode.
    """

    document = pymupdf.open()
    document.new_page(width=100, height=100)
    document.save(path)
    document.close()

    with pikepdf.open(path, allow_overwriting_input=True) as pdf:
        broken = pikepdf.Stream(pdf, b"not deflate data")
        broken.Type, broken.Subtype = Name.XObject, Name.Image
        broken.Width, broken.Height, broken.BitsPerComponent = 4, 4, 8
        broken.ColorSpace, broken.Filter = Name.DeviceGray, Name.FlateDecode
        spot = pikepdf.Stream(pdf, bytes(range(16)))
        spot.Type, spot.Subtype = Name.XObject, Name.Image
        spot.Width, spot.Height, spot.BitsPerComponent = 4, 4, 8
        spot.ColorSpace = pikepdf.Array(
            [
                Name.Separation,
                Name("/Spot"),
                Name.DeviceGray,
                pikepdf.Dictionary(FunctionType=2, Domain=[0, 1], C0=[1], C1=[0], N=1),
            ]
        )
        page = pdf.pages[0]
        page.Resources = pikepdf.Dictionary(
            XObject=pikepdf.Dictionary(Im0=broken, Im1=spot)
        )
        page.Contents = pikepdf.Stream(
            pdf, b"q 40 0 0 40 10 10 cm /Im0 Do Q q 40 0 0 40 50 50 cm /Im1 Do Q"
        )
        pdf.save(path)
    return path
