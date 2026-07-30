package rootfs

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"path/filepath"
	"strings"
)

const (
	_manifestJSON = "manifest.json"
	_tarSuffix    = ".tar"
	_whReaddir    = ".wh..wh..opq"
	_whPrefix     = ".wh."
)

var _gzipMagic = []byte{0x1f, 0x8b}

type (
	dockerManifestJSON []struct {
		Layers []string `json:"Layers"`
	}

	nameOffset struct {
		name   string
		offset int64
	}
)

// Flatten flattens a docker image to a tarball. The underlying io.Writer
// should be an open file handle, which the caller is responsible for closing
// themselves
func Flatten(rd io.ReadSeeker, w io.Writer) (_err error) {
	tr := tar.NewReader(rd)
	var closer func() error
	var err error

	// fileOffsets maps a file name (a9b123c0daa/layer.tar) to it's offset
	fileOffsets := map[string]int64{}

	// manifest is the docker manifest in the image
	var manifest dockerManifestJSON

	// get layer offsets and manifest.json
	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return err
		}
		if hdr.Typeflag != tar.TypeReg {
			continue
		}
		here, err := rd.Seek(0, io.SeekCurrent)
		if err != nil {
			return err
		}
		fileOffsets[strings.TrimPrefix(hdr.Name, "./")] = here
		if filepath.Clean(hdr.Name) == _manifestJSON {
			dec := json.NewDecoder(tr)
			if err := dec.Decode(&manifest); err != nil {
				return fmt.Errorf("decode %s: %w", _manifestJSON, err)
			}
		}
	}

	if err := validateManifest(fileOffsets, manifest); err != nil {
		return err
	}

	// enumerate layers the way they would be laid down in the image
	layers := make([]nameOffset, len(manifest[0].Layers))
	for i, name := range manifest[0].Layers {
		layers[i] = nameOffset{
			name:   name,
			offset: fileOffsets[strings.TrimPrefix(name, "./")],
		}
	}

	// file2layer maps a filename to layer number (index in "layers")
	file2layer := map[string]int{}

	// whreaddir maps `wh..wh..opq` file to a layer; see doc.go
	whreaddir := map[string]int{}

	// wh maps a filename to a layer until which it should be ignored,
	// inclusively; see doc.go
	wh := map[string]int{}

	// iterate over all files, construct `file2layer`, `whreaddir`, `wh`
	for i, no := range layers {
		if _, err := rd.Seek(no.offset, io.SeekStart); err != nil {
			return err
		}
		tr, closer, err = openTargz(rd)
		if err != nil {
			return err
		}
		for {
			hdr, err := tr.Next()
			if err == io.EOF {
				break
			}
			if err != nil {
				return fmt.Errorf("decode %s: %w", no.name, err)
			}
			if hdr.Typeflag == tar.TypeDir {
				continue
			}

			// according to aufs documentation, whiteout files should be
			// hardlinks. I saw at least one docker container using regular
			// files for whiteouts.
			if hdr.Typeflag == tar.TypeLink || hdr.Typeflag == tar.TypeReg {
				basename := filepath.Base(hdr.Name)
				basedir := filepath.Dir(hdr.Name)
				if basename == _whReaddir {
					whreaddir[basedir] = i
					continue
				} else if strings.HasPrefix(basename, _whPrefix) {
					fname := strings.TrimPrefix(basename, _whPrefix)
					wh[filepath.Join(basedir, fname)] = i
					continue
				}
			}
			file2layer[hdr.Name] = i
		}
		if err := closer(); err != nil {
			return err
		}
	}

	// construct directories to whiteout, for each layer.
	whIgnore := whiteoutDirs(whreaddir, len(layers))

	tw := tar.NewWriter(w)
	defer func() {
		_err = errors.Join(_err, tw.Close())
	}()
	// iterate through all layers, all files, and write files.
	for i, no := range layers {
		if _, err := rd.Seek(no.offset, io.SeekStart); err != nil {
			return err
		}
		tr, closer, err = openTargz(rd)
		if err != nil {
			return err
		}
		for {
			hdr, err := tr.Next()
			if err == io.EOF {
				break
			}
			if err != nil {
				return fmt.Errorf("decode %s: %w", no.name, err)
			}
			if layer, ok := wh[hdr.Name]; ok && layer >= i {
				continue
			}
			if whIgnore[i].HasPrefix(hdr.Name) {
				continue
			}
			if hdr.Typeflag != tar.TypeDir && file2layer[hdr.Name] != i {
				continue
			}
			if err := writeFile(tr, tw, hdr); err != nil {
				return err
			}
		}
		if err := closer(); err != nil {
			return err
		}
	}
	return nil
}

func writeFile(tr *tar.Reader, tw *tar.Writer, hdr *tar.Header) error {
	hdrOut := &tar.Header{
		Typeflag: hdr.Typeflag,
		Name:     hdr.Name,
		Linkname: hdr.Linkname,
		Size:     hdr.Size,
		Mode:     int64(hdr.Mode & 0777),
		Uid:      hdr.Uid,
		Gid:      hdr.Gid,
		Uname:    hdr.Uname,
		Gname:    hdr.Gname,
		ModTime:  hdr.ModTime,
		Devmajor: hdr.Devmajor,
		Devminor: hdr.Devminor,
		Format:   tar.FormatGNU,
	}

	if err := tw.WriteHeader(hdrOut); err != nil {
		return err
	}

	if hdr.Typeflag == tar.TypeReg {
		if _, err := io.Copy(tw, tr); err != nil {
			return err
		}
	}

	return nil
}

func whiteoutDirs(whreaddir map[string]int, nlayers int) []*tree {
	ret := make([]*tree, nlayers)
	for i := range ret {
		ret[i] = newTree()
	}
	for fname, layer := range whreaddir {
		if layer == 0 {
			continue
		}
		ret[layer-1].Add(fname)
	}
	for i := nlayers - 1; i > 0; i-- {
		ret[i-1].Merge(ret[i])
	}
	return ret
}

// validateManifest
func validateManifest(
	fileOffsets map[string]int64,
	manifest dockerManifestJSON,
) error {
	if len(manifest) == 0 {
		return fmt.Errorf("empty or missing manifest")
	}

	for _, layer := range manifest[0].Layers {
		if _, ok := fileOffsets[layer]; !ok {
			return fmt.Errorf("%s defined in manifest, missing in tarball", layer)
		}
	}

	return nil
}

// openTargz creates a tar reader from a targzip or tar.
func openTargz(rs io.ReadSeeker) (*tar.Reader, func() error, error) {
	// find out whether the given file is targz or tar
	head := make([]byte, 2)
	_, err := io.ReadFull(rs, head)
	switch {
	case err == io.ErrUnexpectedEOF:
		return nil, nil, errors.New("tarball or gzipfile too small")
	case err != nil:
		return nil, nil, fmt.Errorf("read error: %w", err)
	}

	if _, err := rs.Seek(-2, io.SeekCurrent); err != nil {
		return nil, nil, fmt.Errorf("seek: %w", err)
	}

	r := rs.(io.Reader)
	closer := func() error { return nil }
	if bytes.Equal(head, _gzipMagic) {
		gzipr, err := gzip.NewReader(r)
		if err != nil {
			return nil, nil, fmt.Errorf("gzip.NewReader: %w", err)
		}
		closer = gzipr.Close
		r = gzipr
	}

	return tar.NewReader(r), closer, nil
}
