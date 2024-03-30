RAWBASE="https://raw.pixls.us/data/"
FILELIST = File.expand_path('filelist.sha1', File.dirname(__FILE__))
FILENUM = File.open(FILELIST).each.count

FILES = {}
File.open(FILELIST).each_with_index do |line, i|
  lineparts = line.split("*")
  hash = lineparts[0].strip
  location = lineparts[1].strip
  filedir = File.expand_path("files", File.dirname(__FILE__))
  file = File.expand_path(hash, filedir)
  FILES[hash] = [file,location]
end

def download_file(hash, file, location)
  existhash = nil
  if File.exists?(file)
    existhash = IO.popen("sha1sum \"#{file}\"").read.split(" ")[0]
  end
  if existhash != hash
    puts "Downloading file \"#{file}\"!"
    system "curl -g -f -# \"#{RAWBASE+location.gsub(" ","%20")}\" --create-dirs -o \"#{file}\""
    newhash = IO.popen("sha1sum \"#{file}\"").read.split(" ")[0]
    if newhash != hash
      $stderr.puts "== Download checksum failed, aborting run"
      exit 2
    end
  end
end
